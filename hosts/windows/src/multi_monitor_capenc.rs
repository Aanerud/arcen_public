//! Multi-monitor `capenc` supervision: one dedicated capture worker per
//! applied monitor, reusing the existing single-output implementation
//! (`crate::capenc`) for spawn/READY-validation/keyframe/shutdown so this
//! module never re-implements the `capenc` wire protocol or spawns
//! DXGI/NVENC/Media-Foundation resources itself.
//!
//! Startup is atomic: [`MultiCapencSupervisor::start`] spawns pipelines in
//! plan order and, the moment any one worker fails to start/reach READY,
//! shuts down every worker started so far before returning an error. No
//! partial multi-monitor session is ever left running.
//!
//! # Stable identity through start/restart
//!
//! `capenc`'s argv cannot select an output by stable LUID directly — its
//! actual DDA/NVENC selector is a positional, enumeration-order
//! `output_index` (see `crate::capenc::win::run_with_args`). So a
//! [`MonitorPipelineSpec`] only ever carries a monitor's stable
//! `(adapter_luid, target_id)` binding and mode, never a [`CapencConfig`]:
//! the concrete [`CaptureSelector`]/`CapencConfig` must be re-resolved fresh,
//! via [`resolve_pipeline_specs`], immediately before every start *and*
//! every restart, against a **freshly re-probed** inventory — never a
//! cached one from an earlier enumeration. If a binding no longer resolves
//! to exactly one output (unplugged, or — defensively — ambiguous), the
//! resolution fails closed rather than falling back to a stale selector or
//! matching by `adapter_name` alone (two adapters of the same model can
//! share that string).
//!
//! # Carrier gate
//!
//! The carrier gate is open for Carrier A: each worker remains independently
//! captured/encoded, while `session` fairly multiplexes their monitor-tagged
//! frames over the existing reliable stream.

use arcen_media::{SessionMonitorId, TopologyGeneration};
use arcen_outputs::{start_all_or_rollback, RollbackFailure};
use arcen_protocol::messages::CursorMode;
use arcen_protocol::{ChromaSubsampling, VideoCodec};
use arcen_telemetry::CorrelationId;

use crate::capenc::{Capenc, CapencConfig, CapencStartError, EncoderSelection, IdrRequester};
use crate::multi_monitor_topology::{
    CaptureSelector, CaptureSelectorError, PhysicalOutputInventory, WindowsMonitorPlan,
    WindowsTopologyPlan,
};
use crate::nvapi::AdapterLuid;
use arcen_media::video::ResolvedMediaPlan;
use arcen_media::{BitDepth, ColorMatrix, ColorRange, EncodeIntent};

/// Carrier A and the production Deck consumer are wired.
pub const MULTI_MONITOR_CARRIER_READY: bool = true;

/// Per-session, per-monitor-independent facts shared by every worker this
/// session starts.
#[derive(Debug, Clone)]
pub struct MonitorPipelineTemplate {
    pub codec: VideoCodec,
    pub chroma: ChromaSubsampling,
    pub bit_depth: BitDepth,
    pub color_range: ColorRange,
    pub color_matrix: ColorMatrix,
    pub transfer: arcen_media::TransferCharacteristics,
    pub color_primaries: arcen_media::ColorPrimaries,
    /// Resolved encoder intent every worker in this session requests.
    pub intent: EncodeIntent,
    /// Damage-driven QP biasing every worker in this session requests.
    /// Roster-wide for the same reason the codec is.
    pub qp_map: arcen_media::video::QpMapPolicy,
    pub fps: u32,
    pub encoder: Option<EncoderSelection>,
    pub video_selection: arcen_protocol::messages::VideoSelectionIntent,
    pub cursor_mode: CursorMode,
    pub session_log_id: CorrelationId,
}

/// Typed rejection building [`MonitorPipelineSpec`]s from a
/// [`WindowsTopologyPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiCapencConfigError {
    EmptyPlan,
}

impl std::fmt::Display for MultiCapencConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlan => formatter.write_str("topology plan contains no monitors"),
        }
    }
}

impl std::error::Error for MultiCapencConfigError {}

/// One monitor's stable capture binding and mode, independent of any
/// current-selector snapshot. Deliberately holds no [`CapencConfig`]: unlike
/// the ephemeral `global_index`/`adapter_output_index`/`device_name` a plan
/// captures at planning time, a `CapencConfig` can only be built from a
/// **freshly re-resolved** binding (see [`resolve_pipeline_specs`]), because
/// `capenc`'s argv cannot select an output by LUID directly and a stale
/// current-selector could silently point at the wrong monitor after any
/// display re-enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorPipelineSpec {
    pub session_monitor_id: SessionMonitorId,
    /// Stable identity: see `multi_monitor_topology`'s module documentation.
    pub adapter_luid: AdapterLuid,
    pub target_id: u32,
    /// Rotation-aware applied on-desktop footprint (`WindowsMonitorPlan::width`/
    /// `height` — already swapped from the native mode at 90/270 degrees).
    /// This is the capture/READY geometry; the native pre-rotation mode
    /// (`mode_width`/`mode_height`) is display-set/recovery-journal only and
    /// is never copied here.
    pub width: u32,
    pub height: u32,
    pub encoder: Option<EncoderSelection>,
    pub codec: Option<VideoCodec>,
    pub chroma: Option<ChromaSubsampling>,
}

impl MonitorPipelineSpec {
    pub fn set_media_policy(
        &mut self,
        encoder: EncoderSelection,
        codec: VideoCodec,
        chroma: ChromaSubsampling,
    ) {
        self.encoder = Some(encoder);
        self.codec = Some(codec);
        self.chroma = Some(chroma);
    }
}

/// Builds one [`MonitorPipelineSpec`] per applied monitor in `plan`, in plan
/// order, carrying only each monitor's stable output binding and mode. No
/// [`CapencConfig`] is built here — that requires a freshly re-resolved
/// [`CaptureSelector`] per spec, produced separately by
/// [`resolve_pipeline_specs`] immediately before use.
///
/// # Errors
///
/// Returns an error when the plan has no monitors.
pub fn build_pipeline_specs(
    plan: &WindowsTopologyPlan,
) -> Result<Vec<MonitorPipelineSpec>, MultiCapencConfigError> {
    if plan.monitors.is_empty() {
        return Err(MultiCapencConfigError::EmptyPlan);
    }
    Ok(plan
        .monitors
        .iter()
        .map(|monitor| MonitorPipelineSpec {
            session_monitor_id: monitor.session_monitor_id,
            adapter_luid: monitor.adapter_luid,
            target_id: monitor.target_id,
            // Capture/READY geometry is the rotation-aware applied footprint
            // (already swapped from the native mode at 90/270 degrees), not
            // the pre-rotation `mode_width`/`mode_height` — those remain
            // display-set/recovery-journal only. See `WindowsMonitorPlan`'s
            // field docs in `multi_monitor_topology`.
            width: monitor.width,
            height: monitor.height,
            encoder: None,
            codec: None,
            chroma: None,
        })
        .collect())
}

/// One monitor's [`MonitorPipelineSpec`] plus the [`CaptureSelector`] and
/// complete [`CapencConfig`] freshly resolved from it. Not `Clone`:
/// `CapencConfig` carries a live `CorrelationId` and one-shot process-launch
/// arguments, matching how `capenc::Capenc::spawn` already consumes one
/// owned `CapencConfig` per call.
pub struct ResolvedMonitorPipeline {
    pub session_monitor_id: SessionMonitorId,
    /// The exact stable-to-current binding this pipeline was started/
    /// restarted against. Any subsequent restart of this pipeline must
    /// re-resolve from `(selector.adapter_luid, selector.target_id)` fresh —
    /// never reuse this snapshot's `global_index`/`adapter_output_index`/
    /// `device_name` directly.
    pub selector: CaptureSelector,
    pub config: CapencConfig,
}

impl std::fmt::Debug for ResolvedMonitorPipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `CapencConfig` derives neither `Debug` nor `Clone` (see its own
        // documentation), so this manual impl reports only the resolved
        // selector and elides the config body.
        formatter
            .debug_struct("ResolvedMonitorPipeline")
            .field("session_monitor_id", &self.session_monitor_id)
            .field("selector", &self.selector)
            .field("config", &"<CapencConfig>")
            .finish()
    }
}

/// Typed rejection resolving [`MonitorPipelineSpec`]s against a freshly
/// probed [`PhysicalOutputInventory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiCapencResolveError {
    /// One spec's stable binding failed to resolve against the fresh
    /// inventory (missing or ambiguous — see [`CaptureSelectorError`]).
    Selector {
        session_monitor_id: SessionMonitorId,
        source: CaptureSelectorError,
    },
}

impl std::fmt::Display for MultiCapencResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selector {
                session_monitor_id,
                source,
            } => write!(
                formatter,
                "monitor pipeline {session_monitor_id:?} failed to resolve its stable output binding against the fresh inventory: {source}"
            ),
        }
    }
}

impl std::error::Error for MultiCapencResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Selector { source, .. } => Some(source),
        }
    }
}

/// Resolves every [`MonitorPipelineSpec`] in `specs` against
/// `fresh_inventory` — which must be a **freshly re-probed**
/// [`PhysicalOutputInventory`], never the one a topology plan was originally
/// built against — building each pipeline's concrete [`CaptureSelector`] and
/// complete [`CapencConfig`].
///
/// All-or-nothing: the instant any one spec fails to resolve, the whole call
/// fails closed with no partial results, so a capture start/restart can
/// never proceed on a mix of fresh and stale/guessed bindings.
///
/// # Errors
///
/// Returns [`MultiCapencResolveError`] for the first spec (in `specs` order)
/// whose stable binding does not resolve to exactly one output in
/// `fresh_inventory`.
pub fn resolve_pipeline_specs(
    specs: &[MonitorPipelineSpec],
    fresh_inventory: &PhysicalOutputInventory,
    template: &MonitorPipelineTemplate,
) -> Result<Vec<ResolvedMonitorPipeline>, MultiCapencResolveError> {
    specs
        .iter()
        .map(|spec| {
            let selector = fresh_inventory
                .resolve(spec.adapter_luid, spec.target_id)
                .map_err(|source| MultiCapencResolveError::Selector {
                    session_monitor_id: spec.session_monitor_id,
                    source,
                })?;
            let config = CapencConfig {
                binary: String::new(),
                output_index: selector.global_index,
                adapter_name: Some(selector.adapter_name.clone()),
                adapter_output_index: Some(selector.adapter_output_index),
                device_name: Some(selector.device_name.clone()),
                codec: spec.codec.unwrap_or(template.codec),
                chroma: spec.chroma.unwrap_or(template.chroma),
                bit_depth: if spec.encoder == Some(EncoderSelection::SoftwareH264) {
                    BitDepth::Eight
                } else {
                    template.bit_depth
                },
                color_range: template.color_range,
                color_matrix: if spec.encoder == Some(EncoderSelection::SoftwareH264)
                    && template.color_matrix.is_identity()
                {
                    ColorMatrix::Bt709
                } else {
                    template.color_matrix
                },
                transfer: template.transfer,
                color_primaries: template.color_primaries,
                intent: template.intent,
                qp_map: template.qp_map,
                fps: if spec.encoder == Some(EncoderSelection::SoftwareH264) {
                    template.fps.min(
                        arcen_media::video::EncoderBackend::OpenH264
                            .contract()
                            .max_fps,
                    )
                } else {
                    template.fps
                },
                width: spec.width,
                height: spec.height,
                encoder: spec.encoder.or(template.encoder),
                cursor_mode: template.cursor_mode,
                session_log_id: template.session_log_id.clone(),
            };
            Ok(ResolvedMonitorPipeline {
                session_monitor_id: spec.session_monitor_id,
                selector,
                config,
            })
        })
        .collect()
}

/// Typed rejection from [`MultiCapencSupervisor::start`].
#[derive(Debug)]
pub enum MultiCapencStartError {
    CarrierNotYetEnabled,
    InvalidConfig(MultiCapencConfigError),
    /// One or more specs' stable bindings failed to re-resolve against the
    /// freshly probed inventory passed to [`MultiCapencSupervisor::start`] —
    /// fails closed before any pipeline is spawned.
    ResolveFailed(MultiCapencResolveError),
    PipelineStartFailed {
        session_monitor_id: SessionMonitorId,
        adapter_luid: AdapterLuid,
        target_id: u32,
        global_index: u32,
        failure_index: usize,
        source: CapencStartError,
        rollback_failures: Vec<RollbackFailure<std::convert::Infallible>>,
    },
}

impl std::fmt::Display for MultiCapencStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CarrierNotYetEnabled => formatter
                .write_str("the capenc multi-monitor carrier is not yet enabled on this host"),
            Self::InvalidConfig(error) => {
                write!(
                    formatter,
                    "multi-monitor pipeline configuration is invalid: {error}"
                )
            }
            Self::ResolveFailed(error) => {
                write!(
                    formatter,
                    "failed to resolve fresh capture selectors: {error}"
                )
            }
            Self::PipelineStartFailed {
                session_monitor_id,
                adapter_luid,
                target_id,
                global_index,
                source,
                ..
            } => write!(
                formatter,
                "monitor pipeline {session_monitor_id:?} (adapter {adapter_luid:?}, target {target_id}, output {global_index}) failed to start: {source}"
            ),
        }
    }
}

impl std::error::Error for MultiCapencStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::ResolveFailed(error) => Some(error),
            Self::PipelineStartFailed { source, .. } => Some(source),
            Self::CarrierNotYetEnabled => None,
        }
    }
}

impl From<MultiCapencConfigError> for MultiCapencStartError {
    fn from(error: MultiCapencConfigError) -> Self {
        Self::InvalidConfig(error)
    }
}

impl From<MultiCapencResolveError> for MultiCapencStartError {
    fn from(error: MultiCapencResolveError) -> Self {
        Self::ResolveFailed(error)
    }
}

/// One running monitor pipeline's routing plus its opaque handle (a real
/// [`CapencPipelineHandle`] in production, an injectable fake in tests).
///
/// `selector` is the exact stable-to-current binding this pipeline was
/// started against; any future restart of this pipeline must re-resolve
/// `(selector.adapter_luid, selector.target_id)` against a freshly probed
/// inventory rather than reuse this snapshot's `global_index`/
/// `adapter_output_index`/`device_name` directly (a display re-enumeration
/// can change them at any time).
#[derive(Debug)]
struct MonitorPipeline<H> {
    session_monitor_id: SessionMonitorId,
    selector: CaptureSelector,
    handle: H,
}

/// A running `capenc` worker plus its resolved media plan, held per monitor.
pub struct CapencPipelineHandle {
    pub capenc: Capenc,
    pub frames: std::sync::Arc<crate::latest::VideoQueue<crate::capenc::EncodedFrame>>,
    pub initial_frame: crate::capenc::EncodedFrame,
    pub plan: ResolvedMediaPlan,
}

/// One atomically-started monitor pipeline transferred to the live session.
pub struct StartedMonitorPipeline {
    pub session_monitor_id: SessionMonitorId,
    pub selector: CaptureSelector,
    pub capenc: Capenc,
    pub frames: std::sync::Arc<crate::latest::VideoQueue<crate::capenc::EncodedFrame>>,
    pub initial_frame: crate::capenc::EncodedFrame,
    pub plan: ResolvedMediaPlan,
}

/// Supervises one `capenc` worker per applied monitor for one session.
///
/// Construction is atomic (see [`arcen_outputs::start_all_or_rollback`]); once built, every
/// pipeline in [`Self::routes`] is independently running and READY. A single
/// monitor's transient failure must restart on the *same* output/backend
/// binding it already started with — see [`decide_restart`] — never a hidden
/// backend switch or subset restart; a full session-wide failure must
/// instead call [`Self::shutdown`] and restart every pipeline from a fresh
/// [`MultiCapencSupervisor::start`].
pub struct MultiCapencSupervisor {
    generation: TopologyGeneration,
    pipelines: Vec<MonitorPipeline<CapencPipelineHandle>>,
}

impl MultiCapencSupervisor {
    /// Starts one `capenc` worker per spec, atomically.
    ///
    /// `fresh_inventory` must be a **freshly re-probed**
    /// [`PhysicalOutputInventory`] — never the one a topology plan was
    /// originally built against — since it is what every `spec`'s stable
    /// `(adapter_luid, target_id)` binding is re-resolved against
    /// immediately before spawn, via [`resolve_pipeline_specs`]. This is
    /// what makes spawn *require* a validated, current [`CaptureSelector`]
    /// per pipeline rather than silently degrade to a stale or positional
    /// one.
    ///
    /// # Errors
    ///
    /// Returns [`MultiCapencStartError::CarrierNotYetEnabled`] unconditionally
    /// while [`MULTI_MONITOR_CARRIER_READY`] is `false` (today, always);
    /// [`MultiCapencStartError::ResolveFailed`] when any spec's stable
    /// binding is missing/ambiguous in `fresh_inventory` (fails closed before
    /// any pipeline is spawned); or
    /// [`MultiCapencStartError::PipelineStartFailed`] when any worker fails to
    /// spawn or reach READY — in which case every previously started worker
    /// in this call has already been shut down before the error is returned.
    pub async fn start(
        generation: TopologyGeneration,
        specs: &[MonitorPipelineSpec],
        fresh_inventory: &PhysicalOutputInventory,
        template: &MonitorPipelineTemplate,
    ) -> Result<Self, MultiCapencStartError> {
        if !MULTI_MONITOR_CARRIER_READY {
            return Err(MultiCapencStartError::CarrierNotYetEnabled);
        }
        let resolved = resolve_pipeline_specs(specs, fresh_inventory, template)?;
        let pipelines = start_all_or_rollback(
            resolved,
            |pipeline| {
                let session_monitor_id = pipeline.session_monitor_id;
                let selector = pipeline.selector;
                let error_context = (
                    session_monitor_id,
                    selector.adapter_luid,
                    selector.target_id,
                    selector.global_index,
                );
                async move {
                    let result = async {
                        let (capenc, frames, plan) = Capenc::spawn(pipeline.config).await?;
                        let initial_frame =
                            tokio::time::timeout(std::time::Duration::from_secs(10), frames.pop())
                                .await
                                .map_err(|_| {
                                    CapencStartError::Fatal(
                                        "capenc produced no startup IDR within 10 seconds"
                                            .to_string(),
                                    )
                                })?
                                .ok_or_else(|| {
                                    CapencStartError::Fatal(
                                        "capenc frame queue closed before the startup IDR"
                                            .to_string(),
                                    )
                                })?;
                        if !initial_frame.keyframe {
                            return Err(CapencStartError::Fatal(
                                "capenc startup frame was not an IDR".to_string(),
                            ));
                        }
                        Ok(MonitorPipeline {
                            session_monitor_id,
                            selector,
                            handle: CapencPipelineHandle {
                                capenc,
                                frames,
                                initial_frame,
                                plan,
                            },
                        })
                    }
                    .await;
                    result.map_err(|source| {
                        (
                            error_context.0,
                            error_context.1,
                            error_context.2,
                            error_context.3,
                            source,
                        )
                    })
                }
            },
            |pipeline: MonitorPipeline<CapencPipelineHandle>| async move {
                tracing::warn!(
                    target: crate::logging::CAPENC,
                    global_index = pipeline.selector.global_index,
                    "rolling back monitor pipeline after a sibling pipeline failed to start"
                );
                let mut handle = pipeline.handle;
                handle.capenc.shutdown().await;
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .await
        .map_err(|failure| {
            let failure_index = failure.index();
            let rollback_failures = failure.rollback_failures().to_vec();
            let (session_monitor_id, adapter_luid, target_id, global_index, source) =
                failure.into_start_error();
            MultiCapencStartError::PipelineStartFailed {
                session_monitor_id,
                adapter_luid,
                target_id,
                global_index,
                failure_index,
                source,
                rollback_failures,
            }
        })?;
        tracing::info!(
            target: crate::logging::CAPENC,
            monitors = pipelines.len(),
            "multi-monitor capenc pipelines started"
        );
        Ok(Self {
            generation,
            pipelines,
        })
    }

    /// Returns the session monitor id and current whole-desktop DXGI
    /// enumeration ordinal (`CaptureSelector::global_index`, as of this
    /// pipeline's last start/restart resolution — never a dense per-session
    /// counter) of every currently running pipeline, in start order.
    #[must_use]
    pub fn routes(&self) -> Vec<(SessionMonitorId, u32)> {
        self.pipelines
            .iter()
            .map(|pipeline| (pipeline.session_monitor_id, pipeline.selector.global_index))
            .collect()
    }

    /// Returns the stable `(adapter_luid, target_id)` binding one running
    /// pipeline was last started/restarted against, or `None` when
    /// `session_monitor_id` is not running. Any future restart-execution
    /// path must re-resolve this pair against a freshly probed inventory via
    /// [`resolve_pipeline_specs`]/[`PhysicalOutputInventory::resolve`] —
    /// never reuse the returned [`CaptureSelector`]'s current-selector
    /// fields directly, since a display re-enumeration can change them at
    /// any time.
    #[must_use]
    pub fn stable_binding_for(
        &self,
        session_monitor_id: SessionMonitorId,
    ) -> Option<(AdapterLuid, u32)> {
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.session_monitor_id == session_monitor_id)
            .map(|pipeline| (pipeline.selector.adapter_luid, pipeline.selector.target_id))
    }

    #[must_use]
    pub const fn generation(&self) -> TopologyGeneration {
        self.generation
    }

    /// Transfers ownership of every running pipeline to the attachment.
    #[must_use]
    pub fn into_pipelines(self) -> Vec<StartedMonitorPipeline> {
        self.pipelines
            .into_iter()
            .map(|pipeline| StartedMonitorPipeline {
                session_monitor_id: pipeline.session_monitor_id,
                selector: pipeline.selector,
                capenc: pipeline.handle.capenc,
                frames: pipeline.handle.frames,
                initial_frame: pipeline.handle.initial_frame,
                plan: pipeline.handle.plan,
            })
            .collect()
    }

    /// Returns a cloneable keyframe-request handle for one monitor's
    /// pipeline, or `None` when `session_monitor_id` is not running.
    #[must_use]
    pub fn idr_for(&self, session_monitor_id: SessionMonitorId) -> Option<IdrRequester> {
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.session_monitor_id == session_monitor_id)
            .map(|pipeline| pipeline.handle.capenc.idr())
    }

    /// Shuts down every pipeline. Always terminates every worker even if one
    /// shutdown observes an error internally (each `Capenc::shutdown` call is
    /// itself already best-effort/idempotent-safe).
    pub async fn shutdown(mut self) {
        for pipeline in &mut self.pipelines {
            pipeline.handle.capenc.shutdown().await;
        }
    }
}

/// Bounded decision for whether a single monitor pipeline that just failed
/// should retry on the same output/backend binding it already started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Retry on the same backend/output binding. `attempt` is the 1-based
    /// attempt number about to be made.
    RetrySameBackend { attempt: u32 },
    /// The bounded retry budget for this pipeline is exhausted; the caller
    /// must fail the pipeline (and, per this tranche's atomic-session
    /// contract, tear down the whole session rather than attempt a partial
    /// subset restart).
    ExhaustedGiveUp,
    /// `pipeline_generation` no longer matches the session's current
    /// [`TopologyGeneration`] (a newer topology has since committed); this
    /// failure belongs to a superseded pipeline and must be ignored rather
    /// than acted on.
    StaleGeneration,
}

/// Maximum same-backend restart attempts a single monitor pipeline may make
/// before this decision gives up on it.
pub const MAX_SAME_BACKEND_RESTART_ATTEMPTS: u32 = 3;

/// Decides whether a monitor pipeline that just failed should retry on the
/// same backend, give up (bounded attempts exhausted), or be ignored as
/// stale (its generation no longer matches the session's current one).
///
/// Pure and total: never panics, always returns a decision.
#[must_use]
pub fn decide_restart(
    current_generation: TopologyGeneration,
    pipeline_generation: TopologyGeneration,
    previous_attempts: u32,
) -> RestartDecision {
    if pipeline_generation != current_generation {
        return RestartDecision::StaleGeneration;
    }
    if previous_attempts < MAX_SAME_BACKEND_RESTART_ATTEMPTS {
        RestartDecision::RetrySameBackend {
            attempt: previous_attempts + 1,
        }
    } else {
        RestartDecision::ExhaustedGiveUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_monitor_topology::AvailableOutput;
    use crate::nvapi::AdapterLuid;
    use arcen_media::Rotation;
    use std::sync::{Arc, Mutex};

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("nonzero session monitor id")
    }

    fn luid(low_part: u32) -> AdapterLuid {
        AdapterLuid {
            low_part,
            high_part: 0,
        }
    }

    fn generation(value: u64) -> TopologyGeneration {
        TopologyGeneration::new(value).expect("nonzero generation")
    }

    fn monitor_plan(session_monitor_id: u16, target_id: u32, primary: bool) -> WindowsMonitorPlan {
        WindowsMonitorPlan {
            session_monitor_id: sid(session_monitor_id),
            client_display_id: format!("display-{session_monitor_id}"),
            adapter_luid: luid(1),
            target_id,
            adapter_output_index: target_id,
            adapter_name: "Test Adapter".to_owned(),
            global_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", target_id + 1),
            x: 0,
            y: 0,
            width: 1_920,
            height: 1_080,
            mode_width: 1_920,
            mode_height: 1_080,
            logical_rect: arcen_media::LogicalRect::new(
                arcen_media::LogicalPoint::new(0, 0),
                arcen_media::LogicalSize::from_pixels(1_920, 1_080).expect("logical size"),
            )
            .expect("logical rect"),
            scale: arcen_media::Scale120::new(120).expect("scale"),
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary,
        }
    }

    fn plan(monitor_count: u16) -> WindowsTopologyPlan {
        let monitors = (0..monitor_count)
            .map(|index| monitor_plan(index + 1, u32::from(index), index == 0))
            .collect();
        WindowsTopologyPlan {
            generation: generation(1),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1_920 * u32::from(monitor_count).max(1),
            desktop_height: 1_080,
            monitors,
            requires_custom_timing: false,
        }
    }

    /// An inventory whose entries match `plan`'s monitors' stable
    /// `(adapter_luid, target_id)` bindings exactly, but whose current
    /// selectors (`global_index`/`adapter_output_index`/`device_name`) come
    /// from `current_global_index` instead of the plan's own snapshot — as a
    /// fresh re-probe of the same physical outputs legitimately could
    /// produce after a DXGI re-enumeration.
    fn fresh_inventory_with_global_indices(
        plan: &WindowsTopologyPlan,
        current_global_index: impl Fn(u32) -> u32,
    ) -> PhysicalOutputInventory {
        let outputs = plan
            .monitors
            .iter()
            .map(|monitor| {
                let global_index = current_global_index(monitor.target_id);
                AvailableOutput {
                    adapter_luid: monitor.adapter_luid,
                    target_id: monitor.target_id,
                    adapter_output_index: global_index,
                    adapter_name: monitor.adapter_name.clone(),
                    global_index,
                    device_name: format!(r"\\.\DISPLAY{}", global_index + 1),
                    mode_capability:
                        crate::multi_monitor_topology::OutputModeCapability::CustomTimingCapable {
                            min_width: 320,
                            max_width: 7_680,
                            min_height: 240,
                            max_height: 4_320,
                            min_refresh_hz: 30,
                            max_refresh_hz: 240,
                        },
                    supported_rotations: vec![Rotation::Degrees0],
                    current_x: monitor.x,
                    current_y: monitor.y,
                    current_width: monitor.width,
                    current_height: monitor.height,
                    current_refresh_hz: monitor.refresh_hz,
                    primary: monitor.primary,
                }
            })
            .collect();
        PhysicalOutputInventory::new(outputs).expect("inventory")
    }

    fn fresh_inventory(plan: &WindowsTopologyPlan) -> PhysicalOutputInventory {
        fresh_inventory_with_global_indices(plan, |target_id| target_id)
    }

    fn template() -> MonitorPipelineTemplate {
        MonitorPipelineTemplate {
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            intent: EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 60,
            encoder: Some(EncoderSelection::Auto),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: CursorMode::Local,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    #[test]
    fn grading_quality_intent_reaches_every_monitor_pipeline() {
        let topology = plan(2);
        let specs = build_pipeline_specs(&topology).expect("quality roster specs");
        let inventory = fresh_inventory(&topology);
        let mut template = template();
        template.intent = EncodeIntent::Quality;
        let resolved =
            resolve_pipeline_specs(&specs, &inventory, &template).expect("quality roster");
        assert!(resolved
            .iter()
            .all(|spec| spec.config.intent == EncodeIntent::Quality));
    }

    #[test]
    fn build_pipeline_specs_rejects_an_empty_plan() {
        let empty_plan = WindowsTopologyPlan {
            generation: generation(1),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1,
            desktop_height: 1,
            monitors: Vec::new(),
            requires_custom_timing: false,
        };
        let error = build_pipeline_specs(&empty_plan).expect_err("rejected");
        assert_eq!(error, MultiCapencConfigError::EmptyPlan);
    }

    #[test]
    fn build_pipeline_specs_carries_only_the_stable_binding_no_capenc_config() {
        let plan = plan(3);
        let specs = build_pipeline_specs(&plan).expect("specs");
        assert_eq!(specs.len(), 3);
        for (index, spec) in specs.iter().enumerate() {
            // Specs carry the stable `(adapter_luid, target_id)` binding
            // only; no `CapencConfig`/current selector is built until
            // `resolve_pipeline_specs` re-resolves against a fresh inventory.
            assert_eq!(spec.adapter_luid, luid(1));
            assert_eq!(spec.target_id, index as u32);
        }
    }

    #[test]
    fn build_pipeline_specs_uses_the_rotation_aware_applied_footprint_at_90_degrees() {
        // At 90 degrees the applied on-desktop footprint is the native mode
        // with width/height swapped; capture/READY geometry must be the
        // footprint that is actually on screen (`width`/`height`), never the
        // pre-rotation native mode (`mode_width`/`mode_height`).
        let rotated = WindowsMonitorPlan {
            rotation: Rotation::Degrees90,
            width: 1_080,
            height: 1_920,
            mode_width: 1_920,
            mode_height: 1_080,
            ..monitor_plan(1, 0, true)
        };
        let plan = WindowsTopologyPlan {
            generation: generation(1),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1_080,
            desktop_height: 1_920,
            monitors: vec![rotated],
            requires_custom_timing: false,
        };
        let specs = build_pipeline_specs(&plan).expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].width, 1_080);
        assert_eq!(specs[0].height, 1_920);
    }

    #[test]
    fn build_pipeline_specs_uses_the_rotation_aware_applied_footprint_at_270_degrees() {
        // 270 degrees also swaps width/height from the native mode, on a
        // second (non-primary) monitor placed to the right of the first —
        // exercising rotation and a nonzero signed origin together.
        let rotated = WindowsMonitorPlan {
            rotation: Rotation::Degrees270,
            x: 1_080,
            y: 0,
            width: 1_080,
            height: 1_920,
            mode_width: 1_920,
            mode_height: 1_080,
            ..monitor_plan(2, 1, false)
        };
        let plan = WindowsTopologyPlan {
            generation: generation(1),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 2_160,
            desktop_height: 1_920,
            monitors: vec![rotated],
            requires_custom_timing: false,
        };
        let specs = build_pipeline_specs(&plan).expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].width, 1_080);
        assert_eq!(specs[0].height, 1_920);
    }

    #[test]
    fn resolve_pipeline_specs_carries_the_rotation_aware_footprint_into_the_capenc_config() {
        // End-to-end: a rotated monitor's `CapencConfig.width/height` (the
        // actual capture/READY geometry) must be the applied footprint, not
        // the native mode, all the way through resolution.
        let rotated = WindowsMonitorPlan {
            rotation: Rotation::Degrees90,
            width: 1_080,
            height: 1_920,
            mode_width: 1_920,
            mode_height: 1_080,
            ..monitor_plan(1, 0, true)
        };
        let plan = WindowsTopologyPlan {
            generation: generation(1),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1_080,
            desktop_height: 1_920,
            monitors: vec![rotated],
            requires_custom_timing: false,
        };
        let specs = build_pipeline_specs(&plan).expect("specs");
        let inventory = fresh_inventory(&plan);
        let resolved = resolve_pipeline_specs(&specs, &inventory, &template()).expect("resolved");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].config.width, 1_080);
        assert_eq!(resolved[0].config.height, 1_920);
    }

    #[test]
    fn resolve_pipeline_specs_degrades_mf_depth_and_identity_matrix() {
        let plan = plan(1);
        let mut specs = build_pipeline_specs(&plan).expect("specs");
        specs[0].set_media_policy(
            EncoderSelection::SoftwareH264,
            VideoCodec::H264,
            ChromaSubsampling::Yuv420,
        );
        let inventory = fresh_inventory(&plan);
        let mut template = template();
        template.bit_depth = BitDepth::Ten;
        template.color_range = ColorRange::Full;
        template.color_matrix = ColorMatrix::Identity;
        let resolved = resolve_pipeline_specs(&specs, &inventory, &template)
            .expect("resolved OpenH264 config");
        assert_eq!(resolved[0].config.bit_depth, BitDepth::Eight);
        assert_eq!(resolved[0].config.color_range, ColorRange::Full);
        assert_eq!(resolved[0].config.color_matrix, ColorMatrix::Bt709);
    }

    #[test]
    fn resolve_pipeline_specs_uses_the_fresh_inventorys_current_non_contiguous_global_index() {
        let plan = plan(3);
        let specs = build_pipeline_specs(&plan).expect("specs");
        // A fresh re-probe reports non-contiguous, reversed global indices
        // relative to target id — as a real DXGI re-enumeration could.
        let inventory = fresh_inventory_with_global_indices(&plan, |target_id| 10 - target_id);
        let resolved = resolve_pipeline_specs(&specs, &inventory, &template()).expect("resolved");
        assert_eq!(resolved.len(), 3);
        for (index, pipeline) in resolved.iter().enumerate() {
            let target_id = index as u32;
            let expected_global_index = 10 - target_id;
            assert_eq!(pipeline.selector.global_index, expected_global_index);
            // The concrete `CapencConfig` fed to capenc must use the fresh
            // global index, never the dense per-session position/target id.
            assert_eq!(pipeline.config.output_index, expected_global_index);
        }
    }

    #[test]
    fn resolve_pipeline_specs_resolves_correctly_across_identically_named_adapters() {
        // Two physically distinct adapters sharing the exact same DXGI
        // description string (e.g. two identical GPU models). Resolution
        // must key strictly on `(adapter_luid, target_id)`, never falling
        // back to the shared `adapter_name`.
        let specs = vec![
            MonitorPipelineSpec {
                session_monitor_id: sid(1),
                adapter_luid: luid(1),
                target_id: 0,
                width: 1_920,
                height: 1_080,
                encoder: None,
                codec: None,
                chroma: None,
            },
            MonitorPipelineSpec {
                session_monitor_id: sid(2),
                adapter_luid: luid(2),
                target_id: 0,
                width: 1_920,
                height: 1_080,
                encoder: None,
                codec: None,
                chroma: None,
            },
        ];
        let outputs = vec![
            AvailableOutput {
                adapter_luid: luid(1),
                target_id: 0,
                adapter_output_index: 0,
                adapter_name: "NVIDIA GeForce RTX 4090".to_owned(),
                global_index: 0,
                device_name: r"\\.\DISPLAY1".to_owned(),
                mode_capability:
                    crate::multi_monitor_topology::OutputModeCapability::CustomTimingCapable {
                        min_width: 320,
                        max_width: 7_680,
                        min_height: 240,
                        max_height: 4_320,
                        min_refresh_hz: 30,
                        max_refresh_hz: 240,
                    },
                supported_rotations: vec![Rotation::Degrees0],
                current_x: 0,
                current_y: 0,
                current_width: 1_920,
                current_height: 1_080,
                current_refresh_hz: 60,
                primary: true,
            },
            AvailableOutput {
                adapter_luid: luid(2),
                target_id: 0,
                adapter_output_index: 0,
                adapter_name: "NVIDIA GeForce RTX 4090".to_owned(),
                global_index: 1,
                device_name: r"\\.\DISPLAY2".to_owned(),
                mode_capability:
                    crate::multi_monitor_topology::OutputModeCapability::CustomTimingCapable {
                        min_width: 320,
                        max_width: 7_680,
                        min_height: 240,
                        max_height: 4_320,
                        min_refresh_hz: 30,
                        max_refresh_hz: 240,
                    },
                supported_rotations: vec![Rotation::Degrees0],
                current_x: 1_920,
                current_y: 0,
                current_width: 1_920,
                current_height: 1_080,
                current_refresh_hz: 60,
                primary: false,
            },
        ];
        let inventory = PhysicalOutputInventory::new(outputs).expect("inventory");
        let resolved = resolve_pipeline_specs(&specs, &inventory, &template()).expect("resolved");
        assert_eq!(resolved[0].selector.global_index, 0);
        assert_eq!(resolved[1].selector.global_index, 1);
    }

    #[test]
    fn resolve_pipeline_specs_fails_closed_when_re_enumeration_drops_a_stable_binding() {
        let plan = plan(2);
        let specs = build_pipeline_specs(&plan).expect("specs");
        // The fresh inventory (e.g. after an unplug) no longer contains
        // target id 1 at all.
        let outputs = vec![AvailableOutput {
            adapter_luid: luid(1),
            target_id: 0,
            adapter_output_index: 0,
            adapter_name: "Test Adapter".to_owned(),
            global_index: 0,
            device_name: r"\\.\DISPLAY1".to_owned(),
            mode_capability:
                crate::multi_monitor_topology::OutputModeCapability::CustomTimingCapable {
                    min_width: 320,
                    max_width: 7_680,
                    min_height: 240,
                    max_height: 4_320,
                    min_refresh_hz: 30,
                    max_refresh_hz: 240,
                },
            supported_rotations: vec![Rotation::Degrees0],
            current_x: 0,
            current_y: 0,
            current_width: 1_920,
            current_height: 1_080,
            current_refresh_hz: 60,
            primary: true,
        }];
        let stale_inventory = PhysicalOutputInventory::new(outputs).expect("inventory");
        let error = resolve_pipeline_specs(&specs, &stale_inventory, &template())
            .expect_err("fails closed");
        assert_eq!(
            error,
            MultiCapencResolveError::Selector {
                session_monitor_id: sid(2),
                source: CaptureSelectorError::MissingBinding {
                    adapter_luid: luid(1),
                    target_id: 1,
                },
            }
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeHandle(u32);

    #[tokio::test]
    async fn carrier_gate_is_open_for_the_live_muxed_path() {
        assert!(MULTI_MONITOR_CARRIER_READY);
    }

    fn resolved_pipeline(
        session_monitor_id: u16,
        adapter_luid: AdapterLuid,
        target_id: u32,
        global_index: u32,
    ) -> ResolvedMonitorPipeline {
        let selector = CaptureSelector {
            adapter_luid,
            target_id,
            global_index,
            adapter_name: "Test Adapter".to_owned(),
            adapter_output_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", global_index + 1),
        };
        let config = CapencConfig {
            binary: String::new(),
            output_index: selector.global_index,
            adapter_name: Some(selector.adapter_name.clone()),
            adapter_output_index: Some(selector.adapter_output_index),
            device_name: Some(selector.device_name.clone()),
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            intent: EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 60,
            width: 1_920,
            height: 1_080,
            encoder: Some(EncoderSelection::Auto),
            cursor_mode: CursorMode::Local,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        };
        ResolvedMonitorPipeline {
            session_monitor_id: sid(session_monitor_id),
            selector,
            config,
        }
    }

    #[tokio::test]
    async fn shared_atomic_start_rolls_back_every_started_pipeline_on_a_later_failure() {
        let resolved = vec![
            resolved_pipeline(1, luid(1), 0, 0),
            resolved_pipeline(2, luid(1), 1, 1),
            resolved_pipeline(3, luid(1), 2, 2),
        ];
        let shutdown_calls: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown_calls_for_closure = Arc::clone(&shutdown_calls);
        let result = start_all_or_rollback(
            resolved,
            |pipeline| {
                let output_index = pipeline.selector.global_index;
                async move {
                    // The third (global index 2) pipeline always fails to start.
                    if output_index == 2 {
                        Err::<FakeHandle, _>("boom")
                    } else {
                        Ok(FakeHandle(output_index))
                    }
                }
            },
            move |handle: FakeHandle| {
                let shutdown_calls = Arc::clone(&shutdown_calls_for_closure);
                async move {
                    shutdown_calls.lock().expect("lock").push(handle.0);
                    Ok::<(), &str>(())
                }
            },
        )
        .await;
        let failure = result.expect_err("pipeline 2 must fail");
        assert_eq!(failure.index(), 2);
        assert_eq!(failure.started(), 2);
        assert_eq!(failure.start_error(), &"boom");
        // Pipelines 0 and 1 started before pipeline 2 failed, so both must be
        // rolled back, in reverse start order.
        assert_eq!(*shutdown_calls.lock().expect("lock"), vec![1, 0]);
    }

    #[tokio::test]
    async fn shared_atomic_start_covers_every_failure_index_in_reverse_order() {
        for failure_index in 0..4_u32 {
            let attempts: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
            let attempts_for_spawn = Arc::clone(&attempts);
            let shutdowns: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
            let shutdowns_for_closure = Arc::clone(&shutdowns);
            let result = start_all_or_rollback(
                vec![
                    resolved_pipeline(1, luid(1), 0, 0),
                    resolved_pipeline(2, luid(1), 1, 1),
                    resolved_pipeline(3, luid(1), 2, 2),
                    resolved_pipeline(4, luid(1), 3, 3),
                ],
                move |pipeline| {
                    let output_index = pipeline.selector.global_index;
                    let attempts = Arc::clone(&attempts_for_spawn);
                    async move {
                        attempts.lock().expect("lock").push(output_index);
                        if output_index == failure_index {
                            Err::<FakeHandle, _>("boom")
                        } else {
                            Ok(FakeHandle(output_index))
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
            assert_eq!(failure.start_error(), &"boom");
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
    async fn shared_atomic_start_starts_every_pipeline_when_none_fail() {
        let resolved = vec![
            resolved_pipeline(1, luid(1), 0, 0),
            resolved_pipeline(2, luid(1), 1, 1),
        ];
        let result = start_all_or_rollback(
            resolved,
            |pipeline| async move { Ok::<_, &str>(FakeHandle(pipeline.selector.global_index)) },
            |_handle: FakeHandle| async move { Ok::<(), &str>(()) },
        )
        .await
        .expect("started");
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn shared_atomic_start_preserves_primary_and_every_rollback_error() {
        let shutdowns: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdowns_for_closure = Arc::clone(&shutdowns);
        let result = start_all_or_rollback(
            vec![
                resolved_pipeline(1, luid(1), 0, 0),
                resolved_pipeline(2, luid(1), 1, 1),
                resolved_pipeline(3, luid(1), 2, 2),
                resolved_pipeline(4, luid(1), 3, 3),
            ],
            |pipeline| async move {
                if pipeline.selector.global_index == 3 {
                    Err::<FakeHandle, _>("primary failure")
                } else {
                    Ok(FakeHandle(pipeline.selector.global_index))
                }
            },
            move |handle: FakeHandle| {
                let shutdowns = Arc::clone(&shutdowns_for_closure);
                async move {
                    shutdowns.lock().expect("lock").push(handle.0);
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
            *shutdowns.lock().expect("lock"),
            vec![2, 1, 0],
            "rollback continues after stop failures"
        );
    }

    #[test]
    fn decide_restart_retries_up_to_the_bound_then_gives_up() {
        let current = generation(1);
        assert_eq!(
            decide_restart(current, current, 0),
            RestartDecision::RetrySameBackend { attempt: 1 }
        );
        assert_eq!(
            decide_restart(current, current, MAX_SAME_BACKEND_RESTART_ATTEMPTS - 1),
            RestartDecision::RetrySameBackend {
                attempt: MAX_SAME_BACKEND_RESTART_ATTEMPTS
            }
        );
        assert_eq!(
            decide_restart(current, current, MAX_SAME_BACKEND_RESTART_ATTEMPTS),
            RestartDecision::ExhaustedGiveUp
        );
    }

    #[test]
    fn decide_restart_ignores_a_stale_generation_regardless_of_attempt_count() {
        let current = generation(2);
        let stale = generation(1);
        assert_eq!(
            decide_restart(current, stale, 0),
            RestartDecision::StaleGeneration
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "requires the interactive pier-windows.example.internal desktop and two NVENC outputs"]
    async fn live_current_nvidia_outputs_stream_independently() {
        let inventory = crate::gpu_probe::physical_output_inventory(&[
            "NVIDIA GRID RTX6000-8Q".to_string(),
            "NVIDIA GRID V100D-16Q".to_string(),
        ])
        .expect("physical output inventory");
        assert!(
            inventory.len() >= 2,
            "the live multi-monitor gate requires two NVENC-capable outputs"
        );
        let generation = generation(1);
        let monitors = inventory
            .outputs()
            .iter()
            .take(2)
            .enumerate()
            .map(|(index, output)| WindowsMonitorPlan {
                session_monitor_id: sid(u16::try_from(index + 1).expect("monitor id")),
                client_display_id: format!("live-display-{index}"),
                adapter_luid: output.adapter_luid,
                target_id: output.target_id,
                adapter_output_index: output.adapter_output_index,
                adapter_name: output.adapter_name.clone(),
                global_index: output.global_index,
                device_name: output.device_name.clone(),
                x: output.current_x,
                y: output.current_y,
                width: output.current_width,
                height: output.current_height,
                mode_width: output.current_width,
                mode_height: output.current_height,
                logical_rect: arcen_media::LogicalRect::new(
                    arcen_media::LogicalPoint::from_pixels(
                        i64::from(output.current_x),
                        i64::from(output.current_y),
                    )
                    .expect("logical origin"),
                    arcen_media::LogicalSize::from_pixels(
                        u64::from(output.current_width),
                        u64::from(output.current_height),
                    )
                    .expect("logical size"),
                )
                .expect("logical rect"),
                scale: arcen_media::Scale120::new(120).expect("scale"),
                refresh_hz: output.current_refresh_hz,
                rotation: Rotation::Degrees0,
                primary: index == 0,
            })
            .collect::<Vec<_>>();
        let desktop_x = monitors.iter().map(|monitor| monitor.x).min().unwrap();
        let desktop_y = monitors.iter().map(|monitor| monitor.y).min().unwrap();
        let desktop_right = monitors
            .iter()
            .map(|monitor| monitor.x + i32::try_from(monitor.width).unwrap())
            .max()
            .unwrap();
        let desktop_bottom = monitors
            .iter()
            .map(|monitor| monitor.y + i32::try_from(monitor.height).unwrap())
            .max()
            .unwrap();
        let plan = WindowsTopologyPlan {
            generation,
            desktop_x,
            desktop_y,
            desktop_width: u32::try_from(desktop_right - desktop_x).unwrap(),
            desktop_height: u32::try_from(desktop_bottom - desktop_y).unwrap(),
            monitors,
            requires_custom_timing: false,
        };
        let specs = build_pipeline_specs(&plan).expect("pipeline specs");
        let template = MonitorPipelineTemplate {
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            intent: EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 60,
            encoder: Some(EncoderSelection::Nvenc),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: CursorMode::Local,
            session_log_id: CorrelationId::from_uuid_v4_bytes([7; 16]),
        };
        let supervisor = MultiCapencSupervisor::start(generation, &specs, &inventory, &template)
            .await
            .expect("all live monitor pipelines start");
        let mut pipelines = supervisor.into_pipelines();
        assert_eq!(pipelines.len(), 2);
        for pipeline in &mut pipelines {
            pipeline
                .capenc
                .request_keyframe("live_multi_monitor_round_trip");
            let frame =
                tokio::time::timeout(std::time::Duration::from_secs(30), pipeline.frames.pop())
                    .await
                    .expect("frame timeout")
                    .expect("frame");
            assert!(!frame.data.is_empty());
        }
        for pipeline in &mut pipelines {
            pipeline.capenc.shutdown().await;
        }
    }
}
