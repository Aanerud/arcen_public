//! Multi-monitor `capenc` supervision: one dedicated `capenc` child per
//! applied monitor, reusing the existing single-child implementation
//! (`media::capenc`) for spawn/READY-validation/keyframe/shutdown so this
//! module never re-parses the `capenc` wire protocol.
//!
//! Every monitor's `capenc` child runs in its own OS process, which already
//! gives each pipeline independent bounded output (each child gets its own
//! [`CapencSession`] with its own bounded access-unit channel — see
//! `media::capenc::FRAME_CHANNEL_CAPACITY`) and independent Keel damage
//! -tracking state: Keel (`shared/keel`) runs inside the `arcen-capenc`
//! binary itself (see `hosts/capenc/src/linux_x11.rs`), not in this
//! supervisor, so one child process's private memory is already a fully
//! independent Keel instance — no additional Keel wiring belongs in the pier
//! process.
//!
//! Startup is atomic: [`MultiCapencSupervisor::start`] spawns pipelines in
//! plan order and, the moment any one child fails to start/reach READY,
//! shuts down every pipeline started so far before returning an error. No
//! partial multi-monitor session is ever left running.
//!
//! # Carrier gate
//!
//! [`MULTI_MONITOR_CARRIER_READY`] is a hardcoded constant guarding real
//! multi-monitor delivery independently of the operator gate
//! (`session::multi_monitor::MultiMonitorGate::advertise_enabled`, itself
//! off by default). Carrier A (`muxed_reliable_stream`,
//! `session::monitor_mux::MonitorMux`) keeps every monitor's encoded video
//! multiplexed onto the session's one existing reliable transport stream —
//! it needs no additional transport, only the wiring in this module plus
//! `net::server::run_attachment`. Now that wiring is complete end to end
//! (admission, generated multi-head Xorg config integrated into
//! `session::launcher::DedicatedXorg`, exact RandR verification, one
//! `capenc` child per monitor, the fair `MonitorMux`, and the applied
//! capability published in `ServerHello`), this constant is `true`: the
//! default-off operator gate above is the actual, sole production safety
//! switch — no host ever advertises or admits `multi_monitor_v1` without an
//! operator explicitly opting in via `--multi-monitor`/configuration, no
//! matter what this constant is set to. The supervisor's atomic rollback
//! remains fully unit tested independently of both gates (with an
//! injectable fake spawner/shutdown so the rollback algorithm itself needs
//! no real `capenc` binary or GPU).

use std::path::PathBuf;

#[cfg(test)]
use arcen_media::video::EncoderBackend;
use arcen_media::SessionMonitorId;
use arcen_outputs::{start_all_or_rollback, RollbackFailure};
use arcen_protocol::messages::{CursorMode, MonitorQualityIntentMsg};
use arcen_telemetry::CorrelationId;
use thiserror::Error;

use crate::display::topology::{LinuxTopologyPlan, VALID_HEAD_TOKENS};
use crate::logging::target;
use crate::session::identity::UserExecution;

use super::capenc::{
    self, CapencConfig, CapencSession, CapencStartError, EncoderSelection, IdrRequester,
};
use super::ResolvedMediaPlan;

/// Carrier gate for real multi-monitor `capenc` delivery. See the module
/// documentation: `true` now that Carrier A is fully wired end to end; the
/// operator's own explicit, default-off `--multi-monitor` configuration
/// remains the real, independent production safety switch.
pub const MULTI_MONITOR_CARRIER_READY: bool = true;

/// Per-session, per-monitor-independent facts shared by every pipeline this
/// session starts (binary, encoder policy, X session credentials).
#[derive(Debug, Clone)]
pub struct MonitorPipelineTemplate {
    pub binary: PathBuf,
    pub codec: String,
    pub encoder: EncoderSelection,
    pub fps: u32,
    pub yuv444: bool,
    pub bit_depth: arcen_media::BitDepth,
    pub color_range: arcen_media::ColorRange,
    pub color_matrix: arcen_media::ColorMatrix,
    /// Encoder intent for the whole roster.
    ///
    /// Roster-wide rather than per monitor, for the same reason the codec is:
    /// a monitor encoding to a different budget than its peers would make one
    /// screen of a single desktop feel different from the next.
    pub intent: arcen_media::EncodeIntent,
    /// Damage-driven QP biasing for the whole roster.
    pub qp_map: arcen_media::video::QpMapPolicy,
    pub video_selection: arcen_protocol::messages::VideoSelectionIntent,
    pub cursor_mode: CursorMode,
    pub display: Option<String>,
    pub xauthority: Option<String>,
    pub execution: Option<UserExecution>,
    pub session_log_id: CorrelationId,
}

/// Typed rejection building [`MonitorPipelineSpec`]s from a [`LinuxTopologyPlan`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MultiCapencConfigError {
    #[error("topology plan contains no monitors")]
    EmptyPlan,
    #[error("assigned head {0:?} is not a recognized DFP-N NvFBC output")]
    InvalidHeadToken(String),
    /// This experimental v1's fail-closed encoder-admission contract (see
    /// [`validate_uniform_exact_encoder_policy`]): `auto` is refused for a
    /// committed multi-monitor session outright, because its own
    /// per-attempt NVENC-then-software fallback (`media::capenc::spawn`)
    /// could independently pick a different backend — and, for the
    /// pipelines that fall back, a different clamped geometry — for each
    /// monitor's pipeline, silently diverging from the Xorg RandR geometry
    /// this session already committed to before any pipeline started.
    #[error(
        "multi-monitor sessions must pin an explicit concrete encoder (nvenc or software-h264); \
         auto could silently select a different backend/geometry per monitor after the Xorg \
         multi-head commit"
    )]
    AutoEncoderNotPermitted,
    /// Same contract, other half: an explicit `software-h264` pin is only
    /// safe when *every* monitor's already-committed Xorg geometry fits the
    /// software encoder's contract exactly, with no clamp at all — a
    /// per-monitor clamp here would silently diverge the delivered video
    /// geometry from the Xorg-committed one, exactly as `auto`'s hidden
    /// fallback could.
    #[error(
        "monitor {session_monitor_id:?}'s committed geometry {width}x{height} exceeds the \
         software encoder's exact limits (would be clamped to {fitted_width}x{fitted_height}); \
         pinning software-h264 for a multi-monitor session requires every monitor's committed \
         geometry to be served exactly, never clamped"
    )]
    SoftwareGeometryWouldClamp {
        session_monitor_id: SessionMonitorId,
        width: u32,
        height: u32,
        fitted_width: u32,
        fitted_height: u32,
    },
    #[error(
        "{required} display(s) require full-color NVENC but host policy allows only {limit} hardware session(s)"
    )]
    FullColorExceedsNvencLimit { required: usize, limit: usize },
    #[error(
        "requested {requested} monitors but host policy allows only {limit} NVENC session(s) and software fallback is disabled"
    )]
    SoftwareFallbackDisabled { requested: usize, limit: usize },
}

/// Admission-time (pre-spawn), pure preflight for this experimental v1's
/// fail-closed multi-monitor encoder contract: "committed multi-head
/// geometry must be exactly supported by every selected pipeline before
/// session commit."
///
/// Either the operator has pinned `nvenc` with no fallback, or has pinned
/// `software-h264` and every monitor's already-committed Xorg geometry
/// provably fits the software encoder's own contract exactly (checked here,
/// purely, via [`capenc::fit_to_encoder_limits`] — no process spawned, no
/// display mutated). `auto` and the Windows-only backend are refused
/// outright: see [`MultiCapencConfigError::AutoEncoderNotPermitted`].
///
/// This is the pure, pre-spawn half of the contract; see
/// [`verify_uniform_exact_pipeline_geometry`] for the post-READY half that
/// verifies what every pipeline actually reported once running. Also called
/// from `session::multi_monitor::admit_against_advertised_offer` — earlier
/// still, right after a requested topology is planned but before this host
/// ever calls `SessionRegistry::acquire()`/starts a dedicated Xorg for it —
/// so a policy-incompatible encoder/geometry combination never reaches an
/// Xorg commit at all, not just before `capenc` spawns. `pub(crate)` (not
/// private) for that cross-module admission-time reuse; kept in this module
/// rather than duplicated so the encoder/geometry contract has one
/// implementation.
pub(crate) fn validate_uniform_exact_encoder_policy(
    plan: &LinuxTopologyPlan,
    encoder: EncoderSelection,
) -> Result<(), MultiCapencConfigError> {
    match encoder {
        EncoderSelection::Auto
        | EncoderSelection::WindowsMediaFoundation
        | EncoderSelection::SoftwareAv1 => Err(MultiCapencConfigError::AutoEncoderNotPermitted),
        EncoderSelection::NativeNvenc => Ok(()),
        EncoderSelection::SoftwareH264 => {
            for monitor in &plan.monitors {
                let (fitted_width, fitted_height) = capenc::fit_to_encoder_limits(
                    EncoderSelection::SoftwareH264,
                    monitor.width,
                    monitor.height,
                );
                if fitted_width != monitor.width || fitted_height != monitor.height {
                    return Err(MultiCapencConfigError::SoftwareGeometryWouldClamp {
                        session_monitor_id: monitor.session_monitor_id,
                        width: monitor.width,
                        height: monitor.height,
                        fitted_width,
                        fitted_height,
                    });
                }
            }
            Ok(())
        }
    }
}

/// One monitor's fully resolved `capenc` launch spec: which session monitor
/// it serves, which NvFBC output it captures, and its complete
/// [`CapencConfig`].
#[derive(Debug, Clone)]
pub struct MonitorPipelineSpec {
    pub session_monitor_id: SessionMonitorId,
    /// The `DFP-N` head this monitor was assigned by
    /// `display::topology::plan_topology`, kept only for diagnostics/logging
    /// and to mirror the token `session::xorg_multihead` renders into the
    /// Xorg config — never used to derive [`Self::output_index`].
    pub head: String,
    /// 0-based NvFBC desktop-output index, derived from this monitor's
    /// dense position within the plan-ordered monitor roster so it stays
    /// stable across restarts as long as head assignment does not change.
    pub output_index: u32,
    pub config: CapencConfig,
}

/// Casts a validated plan-roster position (always `<= MAX_MULTI_MONITOR_COUNT`,
/// see `arcen_media::MAX_MULTI_MONITOR_COUNT`) to the `u32` NvFBC expects.
#[allow(clippy::cast_possible_truncation)]
const fn dense_output_index(position: usize) -> u32 {
    position as u32
}

/// Validates that `head` is one of this tranche's recognized `DFP-N`
/// NvFBC-capable output tokens
/// ([`crate::display::topology::VALID_HEAD_TOKENS`]).
///
/// Defensive only: `display::topology::plan_topology` never assigns any
/// other token, but every field crossing from a topology plan into a
/// `capenc` launch spec is still explicitly validated here rather than
/// trusted blindly.
fn validate_head_token(head: &str) -> Result<(), MultiCapencConfigError> {
    if VALID_HEAD_TOKENS.contains(&head) {
        Ok(())
    } else {
        Err(MultiCapencConfigError::InvalidHeadToken(head.to_owned()))
    }
}

/// Builds one [`MonitorPipelineSpec`] per applied monitor in `plan`, in plan
/// order, sharing every session-wide `template` field and taking only
/// per-monitor width/height/output-index from the plan.
///
/// Each spec's `output_index` is this monitor's dense position within
/// `plan.monitors` — the exact same plan-ordered roster
/// `session::xorg_multihead::render_multi_head_xorg_config` renders into the
/// generated Xorg config's `ConnectedMonitor`/`MetaModes` option lines (see
/// `display::topology::plan_topology`'s head-assignment order) — not the
/// assigned `DFP-N` head token's own numeric suffix. NVIDIA's NvFBC
/// enumerates connected outputs densely in that same `ConnectedMonitor`
/// order, so a non-contiguous head selection (e.g. heads `DFP-0` and `DFP-2`
/// assigned together) still yields NvFBC output ordinals `0, 1` — never a
/// gap at `2` — while `DFP-1` and `DFP-3` assigned together likewise yield
/// `0, 1`. Parsing the head token's own suffix instead (e.g. treating
/// `DFP-2` as NvFBC output `2`) would silently point `capenc` at the wrong
/// physical output, or an output that does not exist on this dense screen at
/// all, whenever any head roster is non-contiguous or does not start at
/// `DFP-0`.
///
/// # Errors
///
/// Returns an error when the plan has no monitors or an assigned head is not
/// a recognized `DFP-N` token (defensive: `display::topology::plan_topology`
/// never produces one).
pub fn build_pipeline_specs(
    plan: &LinuxTopologyPlan,
    template: &MonitorPipelineTemplate,
) -> Result<Vec<MonitorPipelineSpec>, MultiCapencConfigError> {
    if plan.monitors.is_empty() {
        return Err(MultiCapencConfigError::EmptyPlan);
    }
    validate_uniform_exact_encoder_policy(plan, template.encoder)?;
    plan.monitors
        .iter()
        .enumerate()
        .map(|(position, monitor)| {
            validate_head_token(&monitor.head)?;
            let output_index = dense_output_index(position);
            Ok(MonitorPipelineSpec {
                session_monitor_id: monitor.session_monitor_id,
                head: monitor.head.clone(),
                output_index,
                config: CapencConfig {
                    binary: template.binary.clone(),
                    output_index,
                    codec: template.codec.clone(),
                    encoder: template.encoder,
                    fps: template.fps,
                    yuv444: template.yuv444,
                    // Auth-time intent is resolved before the topology is
                    // committed, so every monitor receives the same final
                    // session colour contract and the first hello stays true.
                    bit_depth: template.bit_depth,
                    color_range: template.color_range,
                    color_matrix: template.color_matrix,
                    video_selection: template.video_selection,
                    codec_pinned: false,
                    variant_pinned: false,
                    // Encoder intent follows the template for the same reason
                    // the colour axes now do: it is resolved once for the
                    // session, so a monitor added to the roster must not
                    // silently encode to a different budget than its peers.
                    intent: template.intent,
                    qp_map: template.qp_map,
                    width: monitor.width,
                    height: monitor.height,
                    cursor_mode: template.cursor_mode,
                    display: template.display.clone(),
                    xauthority: template.xauthority.clone(),
                    execution: template.execution.clone(),
                    session_log_id: template.session_log_id.clone(),
                },
            })
        })
        .collect()
}

fn nvenc_monitor_ids(
    plan: &LinuxTopologyPlan,
    nvenc_session_limit: Option<u8>,
    allow_software_fallback: bool,
) -> Result<std::collections::BTreeSet<SessionMonitorId>, MultiCapencConfigError> {
    let limit = nvenc_session_limit
        .map_or(plan.monitors.len(), usize::from)
        .min(plan.monitors.len());
    let required = plan
        .monitors
        .iter()
        .filter(|monitor| monitor.quality_intent == MonitorQualityIntentMsg::FullColorRequired)
        .count();
    if required > limit {
        return Err(MultiCapencConfigError::FullColorExceedsNvencLimit { required, limit });
    }
    if plan.monitors.len() > limit && !allow_software_fallback {
        return Err(MultiCapencConfigError::SoftwareFallbackDisabled {
            requested: plan.monitors.len(),
            limit,
        });
    }

    let mut indices = (0..plan.monitors.len()).collect::<Vec<_>>();
    indices.sort_by_key(|index| {
        let monitor = &plan.monitors[*index];
        (
            monitor.quality_intent != MonitorQualityIntentMsg::FullColorRequired,
            !monitor.primary,
            *index,
        )
    });
    let selected = indices
        .into_iter()
        .take(limit)
        .map(|index| plan.monitors[index].session_monitor_id)
        .collect::<std::collections::BTreeSet<_>>();

    for monitor in &plan.monitors {
        if selected.contains(&monitor.session_monitor_id) {
            continue;
        }
        let (fitted_width, fitted_height) = capenc::fit_to_encoder_limits(
            EncoderSelection::SoftwareH264,
            monitor.width,
            monitor.height,
        );
        if fitted_width != monitor.width || fitted_height != monitor.height {
            return Err(MultiCapencConfigError::SoftwareGeometryWouldClamp {
                session_monitor_id: monitor.session_monitor_id,
                width: monitor.width,
                height: monitor.height,
                fitted_width,
                fitted_height,
            });
        }
    }
    Ok(selected)
}

pub fn validate_monitor_resource_policy(
    plan: &LinuxTopologyPlan,
    nvenc_session_limit: Option<u8>,
    allow_software_fallback: bool,
) -> Result<(), MultiCapencConfigError> {
    nvenc_monitor_ids(plan, nvenc_session_limit, allow_software_fallback).map(drop)
}

pub fn build_pipeline_specs_with_resources(
    plan: &LinuxTopologyPlan,
    template: &MonitorPipelineTemplate,
    nvenc_session_limit: Option<u8>,
    allow_software_fallback: bool,
) -> Result<Vec<MonitorPipelineSpec>, MultiCapencConfigError> {
    if template.encoder != EncoderSelection::NativeNvenc {
        return build_pipeline_specs(plan, template);
    }
    let nvenc = nvenc_monitor_ids(plan, nvenc_session_limit, allow_software_fallback)?;
    let mut specs = build_pipeline_specs(plan, template)?;
    for (monitor, spec) in plan.monitors.iter().zip(&mut specs) {
        if nvenc.contains(&monitor.session_monitor_id) {
            spec.config.yuv444 = match monitor.quality_intent {
                MonitorQualityIntentMsg::FullColorRequired => true,
                MonitorQualityIntentMsg::BandwidthOptimized => false,
                MonitorQualityIntentMsg::HostDefault => template.yuv444,
            };
        } else {
            spec.config.encoder = EncoderSelection::SoftwareH264;
            spec.config.codec = "h264".to_string();
            spec.config.yuv444 = false;
            spec.config.fps = spec.config.fps.min(30);
            // Colour must be degraded with the codec, not left at the
            // template's. OpenH264 is 8-bit 4:2:0 BT.709 only, and
            // `RegionMediaPlan::new` validates geometry and fps but not the
            // video config against the backend contract — so a host
            // configured for 10-bit would plan, publish and probe an
            // "OpenH264 at 10 bits" region that cannot exist. It fails closed
            // at pipeline start, but late and with an error that names the
            // wrong cause. Windows already guards exactly this in
            // `multi_monitor_capenc::resolve_pipeline_specs`.
            spec.config.bit_depth = arcen_media::BitDepth::Eight;
            if spec.config.color_matrix.is_identity() {
                // Identity needs 4:4:4, which this monitor just lost.
                spec.config.color_matrix = arcen_media::ColorMatrix::Bt709;
            }
        }
    }
    Ok(specs)
}

/// Typed rejection from [`MultiCapencSupervisor::start`].
#[derive(Debug, Error)]
pub enum MultiCapencStartError {
    #[error("the capenc multi-monitor carrier is not yet enabled on this host")]
    CarrierNotYetEnabled,
    #[error("multi-monitor pipeline configuration is invalid: {0}")]
    InvalidConfig(#[from] MultiCapencConfigError),
    #[error(
        "monitor pipeline {session_monitor_id:?} (head {head}, output {output_index}) failed to start: {source}"
    )]
    PipelineStartFailed {
        session_monitor_id: SessionMonitorId,
        head: String,
        output_index: u32,
        failure_index: usize,
        #[source]
        source: CapencStartError,
        rollback_failures: Vec<RollbackFailure<std::convert::Infallible>>,
    },
}

/// One running monitor pipeline's routing plus its opaque handle (a real
/// [`CapencPipelineHandle`] in production, an injectable fake in tests).
#[derive(Debug)]
struct MonitorPipeline<H> {
    session_monitor_id: SessionMonitorId,
    /// Kept alongside `output_index` purely for diagnostics/logging (see
    /// [`MonitorPipelineSpec::head`]) — never used to route capture traffic.
    head: String,
    output_index: u32,
    handle: H,
}

/// A running `capenc` child plus its resolved media plan, held per monitor.
pub struct CapencPipelineHandle {
    pub session: CapencSession,
    pub plan: ResolvedMediaPlan,
}

/// Supervises one `capenc` child per applied monitor for one session.
///
/// Construction is atomic (see [`arcen_outputs::start_all_or_rollback`]); once built, every
/// pipeline in [`Self::pipelines`] is independently running and READY. Any
/// caller that needs to restart a single monitor's transient failure must do
/// so on the *same* output/backend the pipeline already started with — no
/// hidden backend switch or subset restart is implemented here, matching this
/// tranche's transient-restart constraint; a full session-wide failure must
/// instead call [`Self::shutdown`] and restart every pipeline from a fresh
/// [`MultiCapencSupervisor::start`].
pub struct MultiCapencSupervisor {
    pipelines: Vec<MonitorPipeline<CapencPipelineHandle>>,
}

impl MultiCapencSupervisor {
    /// Starts one `capenc` child per spec, atomically.
    ///
    /// # Errors
    ///
    /// Returns [`MultiCapencStartError::CarrierNotYetEnabled`] unconditionally
    /// while [`MULTI_MONITOR_CARRIER_READY`] is `false` (today, always), or
    /// [`MultiCapencStartError::PipelineStartFailed`] when any child fails to
    /// spawn or reach READY — in which case every previously started child in
    /// this call has already been shut down before the error is returned.
    pub async fn start(specs: Vec<MonitorPipelineSpec>) -> Result<Self, MultiCapencStartError> {
        if !MULTI_MONITOR_CARRIER_READY {
            return Err(MultiCapencStartError::CarrierNotYetEnabled);
        }
        let pipelines = start_all_or_rollback(
            specs,
            |spec| {
                let session_monitor_id = spec.session_monitor_id;
                let head = spec.head.clone();
                let output_index = spec.output_index;
                async move {
                    capenc::spawn(spec.config)
                        .await
                        .map(|(session, plan)| MonitorPipeline {
                            session_monitor_id,
                            head: head.clone(),
                            output_index,
                            handle: CapencPipelineHandle { session, plan },
                        })
                        .map_err(|source| (session_monitor_id, head, output_index, source))
                }
            },
            |pipeline: MonitorPipeline<CapencPipelineHandle>| async move {
                tracing::warn!(
                    target: target::CAPENC,
                    head = pipeline.head,
                    output_index = pipeline.output_index,
                    "rolling back monitor pipeline after a sibling pipeline failed to start"
                );
                pipeline.handle.session.shutdown().await;
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .await
        .map_err(|failure| {
            let failure_index = failure.index();
            let rollback_failures = failure.rollback_failures().to_vec();
            let (session_monitor_id, head, output_index, source) = failure.into_start_error();
            MultiCapencStartError::PipelineStartFailed {
                session_monitor_id,
                head,
                output_index,
                failure_index,
                source,
                rollback_failures,
            }
        })?;
        tracing::info!(
            target: target::CAPENC,
            monitors = pipelines.len(),
            "multi-monitor capenc pipelines started"
        );
        Ok(Self { pipelines })
    }

    /// Returns the session monitor ids and NvFBC output indices of every
    /// currently running pipeline, in start order.
    #[must_use]
    pub fn routes(&self) -> Vec<(SessionMonitorId, u32)> {
        self.pipelines
            .iter()
            .map(|pipeline| (pipeline.session_monitor_id, pipeline.output_index))
            .collect()
    }

    /// Returns a cloneable keyframe-request handle for one monitor's
    /// pipeline, or `None` when `session_monitor_id` is not running.
    #[must_use]
    pub fn idr_for(&self, session_monitor_id: SessionMonitorId) -> Option<IdrRequester> {
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.session_monitor_id == session_monitor_id)
            .map(|pipeline| pipeline.handle.session.idr())
    }

    /// Takes every running pipeline's access-unit frame stream, resolved
    /// media plan, and keyframe-request handle, in start order, so the
    /// caller (`net::server`, Carrier A wiring) can build one
    /// `session::client::FrameQueue` and one generalized frame pump per
    /// monitor and feed them into a `session::monitor_mux::MonitorMux`.
    ///
    /// Each pipeline's underlying frame stream can only be taken once (it is
    /// an `Option` inside `CapencSession`); calling this more than once
    /// yields fewer entries the second time, one per pipeline whose stream
    /// had not already been taken.
    pub fn take_frame_sources(&mut self) -> Vec<MonitorFrameSource> {
        self.pipelines
            .iter_mut()
            .filter_map(|pipeline| {
                let frames = pipeline.handle.session.take_frames()?;
                Some(MonitorFrameSource {
                    session_monitor_id: pipeline.session_monitor_id,
                    head: pipeline.head.clone(),
                    frames,
                    plan: pipeline.handle.plan,
                    idr: pipeline.handle.session.idr(),
                })
            })
            .collect()
    }

    /// Shuts down every pipeline. Always terminates every child even if one
    /// shutdown observes an error internally (each `CapencSession::shutdown`
    /// call is itself already best-effort/idempotent-safe).
    pub async fn shutdown(self) {
        for pipeline in self.pipelines {
            pipeline.handle.session.shutdown().await;
        }
    }
}

/// One running monitor pipeline's frame source, extracted from a
/// [`MultiCapencSupervisor`] for the caller to build a per-monitor
/// `FrameQueue` + frame pump + `MonitorMux`.
pub struct MonitorFrameSource {
    pub session_monitor_id: SessionMonitorId,
    /// Kept for diagnostics/logging only, mirroring
    /// [`MonitorPipelineSpec::head`] — never used for routing.
    pub head: String,
    pub frames: tokio::sync::mpsc::Receiver<crate::media::annexb::AccessUnit>,
    pub plan: ResolvedMediaPlan,
    pub idr: IdrRequester,
}

/// Typed post-READY rejection: what every started pipeline's own resolved
/// [`ResolvedMediaPlan`] must still prove true. See
/// [`verify_uniform_exact_pipeline_geometry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MultiCapencGeometryError {
    #[error(
        "monitor {session_monitor_id:?} resolved capenc geometry \
         {resolved_width}x{resolved_height} does not exactly match its committed Xorg geometry \
         {committed_width}x{committed_height}"
    )]
    GeometryMismatch {
        session_monitor_id: SessionMonitorId,
        committed_width: u32,
        committed_height: u32,
        resolved_width: u32,
        resolved_height: u32,
    },
}

/// Verifies every started pipeline's own resolved [`ResolvedMediaPlan`]
/// exactly matches its monitor's already-committed Xorg/RandR geometry.
/// Mixed backends are intentional when host policy assigns overflow monitors
/// to exact software 4:2:0 pipelines.
///
/// Defense in depth alongside the pure, pre-spawn
/// [`validate_uniform_exact_encoder_policy`] (invoked from
/// [`build_pipeline_specs`]): that preflight already makes a *policy*-driven
/// silent clamp or backend split structurally impossible for the two
/// concrete encoder requests this host permits, but this still verifies
/// what capenc's own child processes actually reported in their READY
/// handshake, rather than trusting the preflight computation alone — a real
/// hardware quirk that clamps one monitor's NVENC geometry, for instance, is
/// still caught here even though no policy asked for it.
///
/// A monitor with no matching started `source` is intentionally skipped
/// here (silently, from this check's own point of view): the caller
/// separately verifies the plan's primary has a matching started pipeline,
/// and the roster/order itself is `session::randr_verify`'s job — this
/// check only ever asserts something about a pipeline that actually exists.
pub fn verify_uniform_exact_pipeline_geometry(
    plan: &LinuxTopologyPlan,
    sources: &[MonitorFrameSource],
) -> Result<(), MultiCapencGeometryError> {
    for monitor in &plan.monitors {
        let Some(source) = sources
            .iter()
            .find(|source| source.session_monitor_id == monitor.session_monitor_id)
        else {
            continue;
        };
        if source.plan.width != monitor.width || source.plan.height != monitor.height {
            return Err(MultiCapencGeometryError::GeometryMismatch {
                session_monitor_id: monitor.session_monitor_id,
                committed_width: monitor.width,
                committed_height: monitor.height,
                resolved_width: source.plan.width,
                resolved_height: source.plan.height,
            });
        }
    }
    Ok(())
}

/// One attachment's active capenc backend: either the legacy single-child
/// session every non-multi-monitor attachment still uses, or a Carrier A
/// [`MultiCapencSupervisor`] running one child per applied monitor.
///
/// Kept as a thin enum (mirroring
/// [`crate::session::monitor_mux::VideoSource`]) with only a unifying
/// [`Self::shutdown`] so every existing single-monitor `capenc.shutdown()`
/// call site in `net::server::run_attachment` keeps working unchanged
/// regardless of which variant is active. Every other single-monitor touch
/// point (`idr`, frame stream, resolved media plan, health/backpressure
/// stats) instead uses the *primary* monitor's own values, extracted once at
/// spawn time — see the module documentation on why a combined multi-future
/// pump is unnecessary.
pub enum CapencHandle {
    Single(CapencSession),
    Multi(MultiCapencSupervisor),
}

impl CapencHandle {
    /// Shuts down the active backend: the single child, or every running
    /// monitor pipeline in the supervisor.
    pub async fn shutdown(self) {
        match self {
            Self::Single(session) => session.shutdown().await,
            Self::Multi(supervisor) => supervisor.shutdown().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::topology::LinuxMonitorPlan;
    use std::sync::{Arc, Mutex};

    /// Validated test-builder for a nonzero [`SessionMonitorId`], centralizing
    /// the one `.expect()` this module's fixtures need instead of scattering
    /// it across every call site.
    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("nonzero session monitor id")
    }

    fn config_for(output_index: u32) -> CapencConfig {
        CapencConfig {
            binary: PathBuf::from("/nonexistent/arcen-capenc"),
            output_index,
            codec: "h264".to_owned(),
            encoder: EncoderSelection::Auto,
            fps: 60,
            yuv444: false,
            bit_depth: arcen_media::BitDepth::Eight,
            color_range: arcen_media::ColorRange::Limited,
            color_matrix: arcen_media::ColorMatrix::Bt709,
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            intent: arcen_media::EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            width: 1920,
            height: 1080,
            cursor_mode: CursorMode::Local,
            display: None,
            xauthority: None,
            execution: None,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    fn spec(session_monitor_id: u16, output_index: u32) -> MonitorPipelineSpec {
        MonitorPipelineSpec {
            session_monitor_id: sid(session_monitor_id),
            head: format!("DFP-{output_index}"),
            output_index,
            config: config_for(output_index),
        }
    }

    fn monitor_plan(session_monitor_id: u16, head: &str, primary: bool) -> LinuxMonitorPlan {
        monitor_plan_sized(session_monitor_id, head, primary, 1920, 1080)
    }

    fn monitor_plan_sized(
        session_monitor_id: u16,
        head: &str,
        primary: bool,
        width: u32,
        height: u32,
    ) -> LinuxMonitorPlan {
        LinuxMonitorPlan {
            session_monitor_id: sid(session_monitor_id),
            client_display_id: format!("display-{session_monitor_id}"),
            head: head.to_owned(),
            x: 0,
            y: 0,
            width,
            height,
            logical_rect: arcen_media::LogicalRect::new(
                arcen_media::LogicalPoint::from_pixels(0, 0).expect("logical origin"),
                arcen_media::LogicalSize::from_pixels(u64::from(width), u64::from(height))
                    .expect("logical size"),
            )
            .expect("logical rect"),
            physical_size: arcen_media::PhysicalSize::new(width, height).expect("physical size"),
            scale: arcen_media::Scale120::new(120).expect("unit scale"),
            rotation: arcen_media::Rotation::Degrees0,
            primary,
            quality_intent: MonitorQualityIntentMsg::BandwidthOptimized,
            mode_token: format!("{width}x{height}"),
        }
    }

    fn template() -> MonitorPipelineTemplate {
        MonitorPipelineTemplate {
            binary: PathBuf::from("/nonexistent/arcen-capenc"),
            codec: "h264".to_owned(),
            // `NativeNvenc`, not `Auto`: these fixtures exercise
            // `build_pipeline_specs`'s head-token/output-index derivation,
            // not its encoder-policy preflight (see
            // `validate_uniform_exact_encoder_policy_*` below for that).
            encoder: EncoderSelection::NativeNvenc,
            fps: 60,
            yuv444: false,
            bit_depth: arcen_media::BitDepth::Eight,
            color_range: arcen_media::ColorRange::Limited,
            color_matrix: arcen_media::ColorMatrix::Bt709,
            intent: arcen_media::EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: CursorMode::Local,
            display: None,
            xauthority: None,
            execution: None,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    fn template_with_encoder(encoder: EncoderSelection) -> MonitorPipelineTemplate {
        MonitorPipelineTemplate {
            encoder,
            ..template()
        }
    }

    fn resolved_plan(backend: EncoderBackend, width: u32, height: u32) -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend,
            video: arcen_media::VideoConfiguration::legacy_h264(),
            width,
            height,
            fps: 60,
            codecs: arcen_media::CodecSet::from_slice(&[arcen_media::VideoCodec::H264]),
            chroma: arcen_media::ChromaSet::from_slice(&[arcen_media::ChromaSubsampling::Yuv420]),
            bit_depths: arcen_media::BitDepthSet::from_slice(&[arcen_media::BitDepth::Eight]),
            ranges: arcen_media::ColorRangeSet::from_slice(&[arcen_media::ColorRange::Limited]),
            cursor_mode: CursorMode::Local,
            cursor_in_video: false,
        }
    }

    fn frame_source(
        session_monitor_id: u16,
        head: &str,
        plan: ResolvedMediaPlan,
    ) -> MonitorFrameSource {
        let (idr, _rx) = capenc::test_support::fake_idr();
        let (_tx, frames) = tokio::sync::mpsc::channel(1);
        MonitorFrameSource {
            session_monitor_id: sid(session_monitor_id),
            head: head.to_owned(),
            frames,
            plan,
            idr,
        }
    }

    #[test]
    fn validate_head_token_accepts_known_tokens_and_rejects_unknown_ones() {
        assert_eq!(validate_head_token("DFP-0"), Ok(()));
        assert_eq!(validate_head_token("DFP-3"), Ok(()));
        assert_eq!(
            validate_head_token("HDMI-0"),
            Err(MultiCapencConfigError::InvalidHeadToken(
                "HDMI-0".to_owned()
            ))
        );
    }

    #[test]
    fn build_pipeline_specs_rejects_an_empty_plan() {
        let plan = LinuxTopologyPlan {
            generation: arcen_media::TopologyGeneration::new(1).expect("generation"),
            virtual_width: 0,
            virtual_height: 0,
            monitors: Vec::new(),
        };
        assert_eq!(
            build_pipeline_specs(&plan, &template()).unwrap_err(),
            MultiCapencConfigError::EmptyPlan
        );
    }

    #[test]
    fn build_pipeline_specs_derives_dense_output_indices_for_non_contiguous_heads_dfp_0_and_dfp_2()
    {
        // Heads DFP-0 and DFP-2 assigned together (DFP-1 unused) must still
        // enumerate as dense NvFBC output ordinals 0 and 1 — not 0 and 2 —
        // while each spec's own `head` token still reports the real DFP-N
        // assignment for diagnostics.
        let plan = LinuxTopologyPlan {
            generation: arcen_media::TopologyGeneration::new(1).expect("generation"),
            virtual_width: 3840,
            virtual_height: 1080,
            monitors: vec![
                monitor_plan(1, "DFP-0", true),
                monitor_plan(2, "DFP-2", false),
            ],
        };
        let specs = build_pipeline_specs(&plan, &template()).expect("specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].head, "DFP-0");
        assert_eq!(specs[0].output_index, 0);
        assert_eq!(specs[0].config.output_index, 0);
        assert_eq!(specs[1].head, "DFP-2");
        assert_eq!(specs[1].output_index, 1);
        assert_eq!(specs[1].config.output_index, 1);
    }

    #[test]
    fn build_pipeline_specs_preserves_the_auth_time_colour_contract_on_every_monitor() {
        let plan = LinuxTopologyPlan {
            generation: arcen_media::TopologyGeneration::new(1).expect("generation"),
            virtual_width: 3840,
            virtual_height: 1080,
            monitors: vec![
                monitor_plan(1, "DFP-0", true),
                monitor_plan(2, "DFP-2", false),
            ],
        };
        let mut template = template();
        template.codec = "h265".to_string();
        template.yuv444 = true;
        template.bit_depth = arcen_media::BitDepth::Ten;
        template.color_range = arcen_media::ColorRange::Full;
        template.video_selection = arcen_protocol::messages::VideoSelectionIntent::ColorFidelity;
        let specs = build_pipeline_specs(&plan, &template).expect("specs");
        assert!(specs.iter().all(|spec| {
            spec.config.codec == "h265"
                && spec.config.yuv444
                && spec.config.bit_depth == arcen_media::BitDepth::Ten
                && spec.config.color_range == arcen_media::ColorRange::Full
                && spec.config.video_selection
                    == arcen_protocol::messages::VideoSelectionIntent::ColorFidelity
        }));
    }

    #[test]
    fn build_pipeline_specs_derives_dense_output_indices_for_non_contiguous_heads_dfp_1_and_dfp_3()
    {
        // Same property, shifted by one head: DFP-1 + DFP-3 (DFP-0/DFP-2
        // unused) must still enumerate as dense NvFBC output ordinals 0, 1.
        let plan = LinuxTopologyPlan {
            generation: arcen_media::TopologyGeneration::new(1).expect("generation"),
            virtual_width: 3840,
            virtual_height: 1080,
            monitors: vec![
                monitor_plan(1, "DFP-1", true),
                monitor_plan(2, "DFP-3", false),
            ],
        };
        let specs = build_pipeline_specs(&plan, &template()).expect("specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].head, "DFP-1");
        assert_eq!(specs[0].output_index, 0);
        assert_eq!(specs[0].config.output_index, 0);
        assert_eq!(specs[1].head, "DFP-3");
        assert_eq!(specs[1].output_index, 1);
        assert_eq!(specs[1].config.output_index, 1);
    }

    #[test]
    fn build_pipeline_specs_rejects_an_unrecognized_head_token() {
        let plan = LinuxTopologyPlan {
            generation: arcen_media::TopologyGeneration::new(1).expect("generation"),
            virtual_width: 1920,
            virtual_height: 1080,
            monitors: vec![monitor_plan(1, "HDMI-0", true)],
        };
        assert_eq!(
            build_pipeline_specs(&plan, &template()).unwrap_err(),
            MultiCapencConfigError::InvalidHeadToken("HDMI-0".to_owned())
        );
    }

    fn two_head_plan_sized(width: u32, height: u32) -> LinuxTopologyPlan {
        LinuxTopologyPlan {
            generation: arcen_media::TopologyGeneration::new(1).expect("generation"),
            virtual_width: width * 2,
            virtual_height: height,
            monitors: vec![
                monitor_plan_sized(1, "DFP-0", true, width, height),
                monitor_plan_sized(2, "DFP-1", false, width, height),
            ],
        }
    }

    #[test]
    fn build_pipeline_specs_rejects_auto_encoder_for_a_multi_monitor_plan() {
        let plan = two_head_plan_sized(1920, 1080);
        assert_eq!(
            build_pipeline_specs(&plan, &template_with_encoder(EncoderSelection::Auto))
                .unwrap_err(),
            MultiCapencConfigError::AutoEncoderNotPermitted
        );
    }

    #[test]
    fn build_pipeline_specs_rejects_windows_media_foundation_encoder_for_a_multi_monitor_plan() {
        let plan = two_head_plan_sized(1920, 1080);
        assert_eq!(
            build_pipeline_specs(
                &plan,
                &template_with_encoder(EncoderSelection::WindowsMediaFoundation)
            )
            .unwrap_err(),
            MultiCapencConfigError::AutoEncoderNotPermitted
        );
    }

    #[test]
    fn build_pipeline_specs_rejects_software_h264_when_a_monitor_would_be_clamped() {
        // 2560x1600 exceeds OpenH264's 1920x1080 contract and would be
        // silently, aspect-preservingly scaled down — the exact scenario
        // issue #3 must fail closed on. The exact fitted values come from
        // `capenc::fit_to_encoder_limits`'s own scaling, not a naive clamp.
        let plan = two_head_plan_sized(2560, 1600);
        let error = build_pipeline_specs(
            &plan,
            &template_with_encoder(EncoderSelection::SoftwareH264),
        )
        .unwrap_err();
        let (fitted_width, fitted_height) =
            capenc::fit_to_encoder_limits(EncoderSelection::SoftwareH264, 2560, 1600);
        assert_ne!(
            (fitted_width, fitted_height),
            (2560, 1600),
            "fixture must actually exercise a real clamp"
        );
        assert_eq!(
            error,
            MultiCapencConfigError::SoftwareGeometryWouldClamp {
                session_monitor_id: sid(1),
                width: 2560,
                height: 1600,
                fitted_width,
                fitted_height,
            }
        );
    }

    #[test]
    fn build_pipeline_specs_accepts_software_h264_when_every_monitor_geometry_is_exactly_within_limits(
    ) {
        // 1920x1080 is exactly OpenH264's contract ceiling: no clamp occurs,
        // so an explicit software-h264 pin must be accepted.
        let plan = two_head_plan_sized(1920, 1080);
        let specs = build_pipeline_specs(
            &plan,
            &template_with_encoder(EncoderSelection::SoftwareH264),
        )
        .expect("exact-fit software geometry must be accepted");
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn build_pipeline_specs_accepts_native_nvenc_unconditionally_at_preflight() {
        // NVENC's real per-GPU limits can't be pre-validated without I/O, so
        // the pure preflight never rejects it on geometry grounds, even at a
        // resolution that would clamp under software-h264.
        let plan = two_head_plan_sized(2560, 1600);
        let specs =
            build_pipeline_specs(&plan, &template_with_encoder(EncoderSelection::NativeNvenc))
                .expect("nvenc pin must be accepted unconditionally at preflight");
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn grading_quality_intent_reaches_every_monitor_pipeline() {
        let plan = two_head_plan_sized(1920, 1080);
        let mut template = template_with_encoder(EncoderSelection::NativeNvenc);
        template.intent = arcen_media::EncodeIntent::Quality;
        let specs = build_pipeline_specs(&plan, &template).expect("quality roster");
        assert!(specs
            .iter()
            .all(|spec| spec.config.intent == arcen_media::EncodeIntent::Quality));
    }

    #[test]
    fn resource_policy_assigns_primary_nvenc_and_exact_secondary_software() {
        let mut plan = two_head_plan_sized(1920, 1080);
        plan.monitors[1].width = 1800;
        plan.monitors[1].height = 1130;
        plan.monitors[1].mode_token = "1800x1130".to_string();
        let specs = build_pipeline_specs_with_resources(&plan, &template(), Some(1), true)
            .expect("exact mixed plan");

        assert_eq!(specs[0].config.encoder, EncoderSelection::NativeNvenc);
        assert!(!specs[0].config.yuv444);
        assert_eq!(specs[1].config.encoder, EncoderSelection::SoftwareH264);
        assert_eq!(specs[1].config.codec, "h264");
        assert!(!specs[1].config.yuv444);
        assert_eq!(specs[1].config.fps, 30);
    }

    #[test]
    fn full_color_secondary_receives_the_only_nvenc_slot() {
        let mut plan = two_head_plan_sized(1920, 1080);
        plan.monitors[1].quality_intent = MonitorQualityIntentMsg::FullColorRequired;
        let mut template = template();
        template.codec = "h265".to_string();
        let specs = build_pipeline_specs_with_resources(&plan, &template, Some(1), true)
            .expect("quality-prioritized mixed plan");

        assert_eq!(specs[0].config.encoder, EncoderSelection::SoftwareH264);
        assert_eq!(specs[1].config.encoder, EncoderSelection::NativeNvenc);
        assert!(specs[1].config.yuv444);
    }

    /// Mirrors Windows' `resolve_pipeline_specs_degrades_mf_depth_and_identity_matrix`.
    /// An overflow monitor pinned to OpenH264 must lose the roster's 10-bit
    /// and identity colour with the codec: OpenH264 is 8-bit 4:2:0 BT.709
    /// only, and nothing downstream re-validates the video config against the
    /// backend contract before the pipeline starts.
    #[test]
    fn software_overflow_monitors_degrade_colour_with_the_codec() {
        let plan = two_head_plan_sized(1920, 1080);
        let mut template = template();
        template.codec = "h265".to_string();
        template.bit_depth = arcen_media::BitDepth::Ten;
        template.color_matrix = arcen_media::ColorMatrix::Identity;
        let specs = build_pipeline_specs_with_resources(&plan, &template, Some(1), true)
            .expect("mixed hardware/software plan");

        assert_eq!(specs[0].config.encoder, EncoderSelection::NativeNvenc);
        assert_eq!(
            specs[0].config.bit_depth,
            arcen_media::BitDepth::Ten,
            "the NVENC monitor keeps the roster contract"
        );

        assert_eq!(specs[1].config.encoder, EncoderSelection::SoftwareH264);
        assert_eq!(
            specs[1].config.bit_depth,
            arcen_media::BitDepth::Eight,
            "OpenH264 cannot encode 10-bit; planning one is a late, misleading failure"
        );
        assert_eq!(
            specs[1].config.color_matrix,
            arcen_media::ColorMatrix::Bt709,
            "identity needs 4:4:4, which this monitor just lost"
        );
    }

    #[test]
    fn resource_policy_rejects_overflow_without_software_fallback() {
        let plan = two_head_plan_sized(1920, 1080);
        assert!(matches!(
            validate_monitor_resource_policy(&plan, Some(1), false),
            Err(MultiCapencConfigError::SoftwareFallbackDisabled {
                requested: 2,
                limit: 1
            })
        ));
    }

    #[test]
    fn verify_uniform_exact_pipeline_geometry_accepts_an_exact_uniform_match() {
        let plan = two_head_plan_sized(1920, 1080);
        let sources = vec![
            frame_source(
                1,
                "DFP-0",
                resolved_plan(EncoderBackend::NativeNvenc, 1920, 1080),
            ),
            frame_source(
                2,
                "DFP-1",
                resolved_plan(EncoderBackend::NativeNvenc, 1920, 1080),
            ),
        ];
        assert_eq!(
            verify_uniform_exact_pipeline_geometry(&plan, &sources),
            Ok(())
        );
    }

    #[test]
    fn verify_uniform_exact_pipeline_geometry_rejects_a_geometry_mismatch() {
        let plan = two_head_plan_sized(1920, 1080);
        let sources = vec![
            frame_source(
                1,
                "DFP-0",
                resolved_plan(EncoderBackend::NativeNvenc, 1920, 1080),
            ),
            // Monitor 2 resolved a narrower geometry than the Xorg commit —
            // a real hardware quirk this check exists to catch.
            frame_source(
                2,
                "DFP-1",
                resolved_plan(EncoderBackend::NativeNvenc, 1280, 720),
            ),
        ];
        assert_eq!(
            verify_uniform_exact_pipeline_geometry(&plan, &sources).unwrap_err(),
            MultiCapencGeometryError::GeometryMismatch {
                session_monitor_id: sid(2),
                committed_width: 1920,
                committed_height: 1080,
                resolved_width: 1280,
                resolved_height: 720,
            }
        );
    }

    #[test]
    fn verify_uniform_exact_pipeline_geometry_accepts_an_exact_mixed_backend() {
        let plan = two_head_plan_sized(1920, 1080);
        let sources = vec![
            frame_source(
                1,
                "DFP-0",
                resolved_plan(EncoderBackend::NativeNvenc, 1920, 1080),
            ),
            frame_source(
                2,
                "DFP-1",
                resolved_plan(EncoderBackend::OpenH264, 1920, 1080),
            ),
        ];
        assert_eq!(
            verify_uniform_exact_pipeline_geometry(&plan, &sources),
            Ok(())
        );
    }

    #[test]
    fn verify_uniform_exact_pipeline_geometry_skips_a_monitor_with_no_started_source() {
        // A monitor whose pipeline never started (already reported through
        // `MultiCapencSupervisor::start`'s own rollback path) has no
        // matching source here; this check only ever asserts something
        // about a pipeline that actually exists.
        let plan = two_head_plan_sized(1920, 1080);
        let sources = vec![frame_source(
            1,
            "DFP-0",
            resolved_plan(EncoderBackend::NativeNvenc, 1920, 1080),
        )];
        assert_eq!(
            verify_uniform_exact_pipeline_geometry(&plan, &sources),
            Ok(())
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeHandle(u32);

    #[tokio::test]
    async fn every_pipeline_starts_when_every_spawn_succeeds() {
        let specs = vec![spec(1, 0), spec(2, 1), spec(3, 2), spec(4, 3)];
        let result = start_all_or_rollback(
            specs,
            |spec| async move { Ok::<_, &str>(FakeHandle(spec.output_index)) },
            |_handle: FakeHandle| async move { Ok::<(), &str>(()) },
        )
        .await
        .expect("every pipeline starts");
        assert_eq!(result.len(), 4);
        assert_eq!(
            result.iter().map(|handle| handle.0).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[tokio::test]
    async fn shared_atomic_start_rolls_back_every_previously_started_sibling_in_reverse_order() {
        let specs = vec![spec(1, 0), spec(2, 1), spec(3, 2), spec(4, 3)];
        let shutdown_log = Arc::new(Mutex::new(Vec::new()));
        let shutdown_log_for_closure = Arc::clone(&shutdown_log);
        let result = start_all_or_rollback(
            specs,
            |spec| async move {
                if spec.output_index == 2 {
                    Err::<FakeHandle, _>("simulated failure")
                } else {
                    Ok(FakeHandle(spec.output_index))
                }
            },
            move |handle: FakeHandle| {
                let log = Arc::clone(&shutdown_log_for_closure);
                async move {
                    log.lock().expect("lock").push(handle.0);
                    Ok::<(), &str>(())
                }
            },
        )
        .await;

        let failure = result.expect_err("pipeline 2 must fail");
        assert_eq!(failure.index(), 2);
        assert_eq!(failure.started(), 2);
        assert_eq!(failure.start_error(), &"simulated failure");
        // Pipelines 0 and 1 already started before pipeline 2 failed; they
        // must be shut down in reverse start order. Pipeline 3 (after the
        // failure) must never have been attempted or shut down.
        assert_eq!(*shutdown_log.lock().expect("lock"), vec![1, 0]);
    }

    #[tokio::test]
    async fn every_failure_index_stops_spawning_and_rolls_back_in_reverse_order() {
        for failure_index in 0..4_u32 {
            let attempts = Arc::new(Mutex::new(Vec::new()));
            let attempts_for_spawn = Arc::clone(&attempts);
            let shutdowns = Arc::new(Mutex::new(Vec::new()));
            let shutdowns_for_closure = Arc::clone(&shutdowns);
            let result = start_all_or_rollback(
                vec![spec(1, 0), spec(2, 1), spec(3, 2), spec(4, 3)],
                move |spec| {
                    let attempts = Arc::clone(&attempts_for_spawn);
                    async move {
                        attempts.lock().expect("lock").push(spec.output_index);
                        if spec.output_index == failure_index {
                            Err::<FakeHandle, _>("simulated failure")
                        } else {
                            Ok(FakeHandle(spec.output_index))
                        }
                    }
                },
                move |handle: FakeHandle| {
                    let shutdowns = Arc::clone(&shutdowns_for_closure);
                    async move {
                        shutdowns.lock().expect("lock").push(handle.0);
                        Ok::<(), &str>(())
                    }
                },
            )
            .await;

            let failure = result.expect_err("selected pipeline must fail");
            assert_eq!(failure.index(), failure_index as usize);
            assert_eq!(failure.started(), failure_index as usize);
            assert_eq!(failure.start_error(), &"simulated failure");
            assert_eq!(
                *attempts.lock().expect("lock"),
                (0..=failure_index).collect::<Vec<_>>()
            );
            assert_eq!(
                *shutdowns.lock().expect("lock"),
                (0..failure_index).rev().collect::<Vec<_>>()
            );
        }
    }

    #[tokio::test]
    async fn shared_atomic_start_preserves_primary_and_every_rollback_error() {
        let specs = vec![spec(1, 0), spec(2, 1), spec(3, 2), spec(4, 3)];
        let shutdown_log = Arc::new(Mutex::new(Vec::new()));
        let shutdown_log_for_closure = Arc::clone(&shutdown_log);
        let result = start_all_or_rollback(
            specs,
            |spec| async move {
                if spec.output_index == 3 {
                    Err::<FakeHandle, _>("primary failure")
                } else {
                    Ok(FakeHandle(spec.output_index))
                }
            },
            move |handle: FakeHandle| {
                let log = Arc::clone(&shutdown_log_for_closure);
                async move {
                    log.lock().expect("lock").push(handle.0);
                    if handle.0 == 2 || handle.0 == 1 {
                        Err(format!("rollback {0} failed", handle.0))
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;
        let failure = result.expect_err("pipeline 3 must fail");
        assert_eq!(failure.index(), 3);
        assert_eq!(failure.started(), 3);
        assert_eq!(failure.start_error(), &"primary failure");
        assert_eq!(
            failure
                .rollback_failures()
                .iter()
                .map(|failure| (failure.index, failure.source.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, "rollback 2 failed"), (1, "rollback 1 failed")]
        );
        assert_eq!(
            *shutdown_log.lock().expect("lock"),
            vec![2, 1, 0],
            "rollback continues after stop failures"
        );
    }

    #[tokio::test]
    async fn start_now_attempts_a_real_spawn_since_the_carrier_gate_is_open() {
        // MULTI_MONITOR_CARRIER_READY is `true` now that Carrier A is fully
        // wired, so `start` no longer fails fast with
        // `CarrierNotYetEnabled` — it genuinely attempts to spawn every
        // configured pipeline. `spec` points at a nonexistent binary, so the
        // real, independently-tested shared atomic-failure
        // path is what surfaces the error here instead.
        let specs = vec![spec(1, 0)];
        let result = MultiCapencSupervisor::start(specs).await;
        assert!(matches!(
            result
                .err()
                .expect("nonexistent capenc binary must fail to spawn"),
            MultiCapencStartError::PipelineStartFailed {
                source: CapencStartError::Spawn(_),
                ..
            }
        ));
    }
}
