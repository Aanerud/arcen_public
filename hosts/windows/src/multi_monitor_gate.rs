//! Pre-auth `multi_monitor_v1` capability offer and post-auth admission
//! gating, owned entirely by this host (analogous to
//! `hosts/linux/src/session/multi_monitor.rs`, adapted to this platform's
//! live-probed rather than pre-declared output inventory — see below).
//!
//! This module is the only place that decides whether this host advertises
//! `multi_monitor_v1` before authentication and whether an authenticated
//! client's requested topology is admitted. It never mutates the live
//! display; it only produces a typed decision that a caller logs and (once
//! the capture-plan carrier lands) would act on.
//!
//! Two independent gates must both be open before this host ever advertises
//! or admits `multi_monitor_v1`:
//!
//! - **Operator gate** ([`MultiMonitorGate::advertise_enabled`] plus a
//!   non-empty [`PhysicalOutputInventory`] supplied by the caller): the
//!   explicit config opt-in this tranche's task requires.
//! - **Carrier gate** ([`crate::multi_monitor_capenc::MULTI_MONITOR_CARRIER_READY`]):
//!   open now that Carrier A multiplexes monitor-tagged frames from the
//!   per-output capture supervisor over the existing reliable stream.
//!
//! # Why the inventory is not stored on the gate
//!
//! Unlike Linux (which pre-declares a fixed dedicated-Xorg head roster in
//! config, baked into the X server at session launch), this host attaches to
//! the *existing* physical desktop, whose attached-output set can only be
//! known by probing it live (`crate::gpu_probe::probe`) at connection time —
//! exactly how the existing single-output path already resolves an output
//! selector. So [`MultiMonitorGate`] carries only the operator opt-in;
//! callers pass the freshly probed [`PhysicalOutputInventory`] into
//! [`build_offer`]/[`admit_requested_topology`] explicitly.
//!
//! Legacy/default behavior (no offer, no request, existing single-primary
//! degrade in `session::session_display_plan`) is completely unaffected: this
//! module only adds a new, additive decision path that a caller must opt
//! into, and it can never make session establishment less safe than today,
//! only refuse to enable the new one.

use arcen_media::video::ResolvedMediaPlan;
use arcen_media::{
    MediaContractError, RegionMediaRoster, RequestedMonitorTopology, Rotation, SessionMonitorId,
    TopologyGeneration, MAX_MULTI_MONITOR_COUNT,
};
use arcen_outputs::admission::{
    admit_regions, AdmissionGates, AdmissionOutcome, DegradeReason, GateClosed,
    RegionAdmissionPolicy,
};
use arcen_outputs::applied::{
    assemble_applied_regions, AppliedRegion, AppliedRegionAssembler, OriginTranslation,
};
use arcen_protocol::messages::{
    AdvertisedMultiMonitorOffer, AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg,
    AppliedMonitorTopologyMsg, AuthMultiMonitorOfferMsg, AuthMultiMonitorRequestMsg, AuthRequest,
    AuthRequestMultiMonitorOfferError, ClientDisplayId, ClientDisplayIdError,
    MultiMonitorCarrierMsg, MultiMonitorValidationError, RequestedMonitorDescriptorMsg,
    RequestedMonitorTopologyMsg, RotationMsg, ServerMultiMonitorMsg, TopologyBackendKindMsg,
};

use crate::config::WindowsMultiMonitorConfig;
use crate::multi_monitor_capenc::MULTI_MONITOR_CARRIER_READY;
use crate::multi_monitor_topology::{
    self, PhysicalOutputInventory, WindowsMonitorPlan, WindowsTopologyError, WindowsTopologyPlan,
};

/// Carrier A is the production-compatible shape shared with Linux and Deck:
/// independent per-output workers feed monitor-tagged frames through the
/// session's existing reliable stream.
const OFFERED_CARRIERS: [MultiMonitorCarrierMsg; 1] = [MultiMonitorCarrierMsg::MuxedReliableStream];

/// Hardware-validated safety ceiling for the native NVIDIA headless provider.
///
/// Two V100D heads pass end to end. A three-head mixed-orientation topology
/// can block indefinitely inside Windows `SetDisplayConfig`, so the offer must
/// never promise more than the provider can safely attempt.
pub const MAX_NVIDIA_HEADLESS_MONITORS: u8 = 2;

/// Explicit operator-facing gate for `multi_monitor_v1`. Defaults to fully
/// disabled; an operator must explicitly set `advertise_enabled` for this
/// host to ever advertise multi-monitor support (and even then, the offer
/// stays withheld while the physical output inventory is empty, or the
/// hardcoded carrier gate stays closed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MultiMonitorGate {
    pub advertise_enabled: bool,
    /// Operator ceiling on the advertised monitor count. `None` advertises
    /// [`MAX_MULTI_MONITOR_COUNT`]; `Some(n)` advertises `min(n,
    /// MAX_MULTI_MONITOR_COUNT)` so a host with a known smaller physical
    /// output count never offers a layout it cannot apply.
    pub max_monitors: Option<u8>,
}

impl MultiMonitorGate {
    /// The fully disabled gate (today's legacy/default behavior).
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            advertise_enabled: false,
            max_monitors: None,
        }
    }

    /// Builds an operator gate from the validated `[platform.multi_monitor]`
    /// config section.
    #[must_use]
    pub const fn from_config(config: &WindowsMultiMonitorConfig) -> Self {
        let max_monitors = if config.nvidia_headless_enabled {
            Some(match config.max_monitors {
                Some(configured) => {
                    if configured < MAX_NVIDIA_HEADLESS_MONITORS {
                        configured
                    } else {
                        MAX_NVIDIA_HEADLESS_MONITORS
                    }
                }
                None => MAX_NVIDIA_HEADLESS_MONITORS,
            })
        } else {
            config.max_monitors
        };
        Self {
            advertise_enabled: config.advertise_enabled,
            max_monitors,
        }
    }
}

/// Outcome of admitting an authenticated client's requested `multi_monitor_v1`
/// topology (if any) against this host's gate and probed output inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiMonitorOutcome {
    /// The client did not send a `multi_monitor_v1` requested topology;
    /// today's legacy/default single-primary behavior applies completely
    /// unchanged.
    NotRequested,
    /// The client requested multi-monitor but this host falls back to its
    /// existing single-primary degrade behavior for a documented, typed
    /// reason. No partial topology is ever applied: admission is atomic —
    /// either every requested monitor plans successfully, or none do.
    Degraded(MultiMonitorDegradeReason),
    /// The requested topology was fully, atomically planned onto this host's
    /// probed physical output inventory, using `carrier` — the host-selected
    /// element of the client/host common carrier intersection computed via
    /// [`AdvertisedMultiMonitorOffer::select_carrier`]. Reaching this variant
    /// already required the carrier gate to be open, so callers may apply it.
    Planned {
        plan: WindowsTopologyPlan,
        carrier: MultiMonitorCarrierMsg,
    },
}

/// Typed reason this host degraded a requested `multi_monitor_v1` topology to
/// the existing single-primary behavior instead of admitting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiMonitorDegradeReason {
    CarrierNotYetEnabled,
    GateDisabled,
    NoInventoryAvailable,
    NotAdvertised,
    ExceedsAdvertisedOffer(MultiMonitorValidationError),
    InvalidRequestedTopology(MediaContractError),
    PlanningFailed(WindowsTopologyError),
}

impl std::fmt::Display for MultiMonitorDegradeReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CarrierNotYetEnabled => {
                formatter.write_str("the multi-monitor carrier is not yet enabled on this host")
            }
            Self::GateDisabled => formatter
                .write_str("the multi_monitor_v1 advertisement gate is disabled on this host"),
            Self::NoInventoryAvailable => formatter.write_str(
                "no physical output inventory is available for multi_monitor_v1 on this host",
            ),
            Self::NotAdvertised => formatter.write_str(
                "client requested multi_monitor_v1 without this host having advertised it",
            ),
            Self::ExceedsAdvertisedOffer(error) => write!(
                formatter,
                "requested topology exceeds this host's advertised multi_monitor_v1 offer: {error}"
            ),
            Self::InvalidRequestedTopology(error) => {
                write!(formatter, "requested topology is invalid: {error}")
            }
            Self::PlanningFailed(error) => write!(
                formatter,
                "requested topology could not be planned onto the probed output inventory: {error}"
            ),
        }
    }
}

impl std::error::Error for MultiMonitorDegradeReason {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ExceedsAdvertisedOffer(error) => Some(error),
            Self::InvalidRequestedTopology(error) => Some(error),
            Self::PlanningFailed(error) => Some(error),
            _ => None,
        }
    }
}

/// Builds this host's pre-auth `multi_monitor_v1` offer for `AuthRequest`, or
/// `None` when any gate is closed (operator gate disabled, no probed
/// inventory supplied/empty, or the carrier gate is not yet open).
///
/// The advertised `max_monitors` is [`MAX_MULTI_MONITOR_COUNT`] unless the
/// operator declared a smaller [`MultiMonitorGate::max_monitors`] ceiling. It
/// is deliberately not derived from a live probe: this offer is built before
/// authentication, in session 0, where this host cannot see the interactive
/// desktop's outputs at all (see the module documentation and
/// `preauth_offer_does_not_require_session_zero_display_inventory`).
#[must_use]
pub fn build_offer(gate: &MultiMonitorGate) -> Option<AuthMultiMonitorOfferMsg> {
    if !MULTI_MONITOR_CARRIER_READY || !gate.advertise_enabled {
        return None;
    }
    let protocol_ceiling = u8::try_from(MAX_MULTI_MONITOR_COUNT).ok()?;
    let max_monitors = gate
        .max_monitors
        .map_or(protocol_ceiling, |operator| operator.min(protocol_ceiling));
    let supported_rotations = RotationMsg::ALL.to_vec();
    AuthMultiMonitorOfferMsg::new(max_monitors, supported_rotations, OFFERED_CARRIERS.to_vec()).ok()
}

/// Admits (or degrades) an authenticated client's optional requested
/// `multi_monitor_v1` topology.
///
/// `offer` must be the exact offer this host attached to the `AuthRequest`
/// that preceded `requested` (or `None` if this host never advertised
/// support this connection). `requested` is the client's `AuthResponse`
/// `multi_monitor_v1` sidecar: the requested topology plus the client's
/// ordered carrier support.
///
/// The carrier gate is checked first and unconditionally: every other check
/// (operator gate, inventory, advertised-offer/carrier validation, planning)
/// only ever runs once [`MULTI_MONITOR_CARRIER_READY`] is `true`, so this
/// host's admitted behavior stays fully primary-only/default-off while that
/// constant stays `false`, regardless of `gate` or what the client requested.
#[must_use]
pub fn admit_requested_topology(
    gate: &MultiMonitorGate,
    inventory: Option<&PhysicalOutputInventory>,
    offer: Option<&AuthMultiMonitorOfferMsg>,
    requested: Option<&AuthMultiMonitorRequestMsg>,
) -> MultiMonitorOutcome {
    admit(gate, inventory, offer, requested, WindowsPlanner::Requested)
}

/// Admits the request against the pre-auth offer, then maps it onto the
/// physical rectangles Windows has already applied in the interactive
/// session. This first live path performs no display mutation.
///
/// Identical to [`admit_requested_topology`] in every gate, offer, carrier
/// and conversion step — the shared driver runs exactly one sequence for both
/// — differing only in which planner this host hands the converted topology
/// to.
#[must_use]
pub fn admit_requested_current_topology(
    gate: &MultiMonitorGate,
    inventory: Option<&PhysicalOutputInventory>,
    offer: Option<&AuthMultiMonitorOfferMsg>,
    requested: Option<&AuthMultiMonitorRequestMsg>,
) -> MultiMonitorOutcome {
    admit(gate, inventory, offer, requested, WindowsPlanner::Current)
}

fn admit(
    gate: &MultiMonitorGate,
    inventory: Option<&PhysicalOutputInventory>,
    offer: Option<&AuthMultiMonitorOfferMsg>,
    requested: Option<&AuthMultiMonitorRequestMsg>,
    planner: WindowsPlanner,
) -> MultiMonitorOutcome {
    let policy = WindowsRegionAdmission::new(gate, inventory, offer, planner);
    admit_regions(policy.gates(), &policy, requested).into()
}

/// Which of this host's two topology planners an admission drives.
///
/// The only difference between this host's two public admission entry
/// points: everything before planning is one shared sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsPlanner {
    /// Plans the requested layout onto the probed inventory.
    Requested,
    /// Maps the request onto the rectangles Windows has already applied.
    Current,
}

/// This host's implementation of the shared multi-region admission steps.
///
/// Holds the `AuthRequest` wrapper for this connection's offer so the
/// advertised-offer evidence is derived once, then borrowed by both the offer
/// and carrier steps.
struct WindowsRegionAdmission<'a> {
    /// `Some` exactly when a non-empty probed inventory was supplied.
    inventory: Option<&'a PhysicalOutputInventory>,
    /// `Some` exactly when this host advertised an offer on this connection.
    offer: Option<AuthRequest>,
    operator_enabled: bool,
    planner: WindowsPlanner,
}

impl<'a> WindowsRegionAdmission<'a> {
    fn new(
        gate: &MultiMonitorGate,
        inventory: Option<&'a PhysicalOutputInventory>,
        offer: Option<&AuthMultiMonitorOfferMsg>,
        planner: WindowsPlanner,
    ) -> Self {
        Self {
            inventory: inventory.filter(|inventory| !inventory.is_empty()),
            offer: offer.map(offer_wrapper),
            operator_enabled: gate.advertise_enabled,
            planner,
        }
    }

    fn gates(&self) -> AdmissionGates {
        AdmissionGates {
            carrier_ready: MULTI_MONITOR_CARRIER_READY,
            operator_enabled: self.operator_enabled,
            inventory_available: self.inventory.is_some(),
            offer_advertised: self.offer.is_some(),
        }
    }

    /// The only public way to obtain `AdvertisedMultiMonitorOffer` evidence is
    /// through `AuthRequest::required_multi_monitor_v1_offer()`; the stored
    /// wrapper always carries `Some(offer)`, and the shared `offer_advertised`
    /// gate already refused a connection without one, so neither `None` here
    /// nor `Missing` below actually triggers (kept as typed, non-panicking
    /// fallbacks rather than an `unreachable!()` in case that invariant ever
    /// changes upstream).
    fn advertised(&self) -> Result<AdvertisedMultiMonitorOffer<'_>, MultiMonitorDegradeReason> {
        let Some(wrapper) = self.offer.as_ref() else {
            return Err(MultiMonitorDegradeReason::NotAdvertised);
        };
        match wrapper.required_multi_monitor_v1_offer() {
            Ok(advertised) => Ok(advertised),
            Err(AuthRequestMultiMonitorOfferError::Missing) => {
                Err(MultiMonitorDegradeReason::NotAdvertised)
            }
            Err(AuthRequestMultiMonitorOfferError::Invalid(error)) => {
                Err(MultiMonitorDegradeReason::ExceedsAdvertisedOffer(error))
            }
        }
    }
}

impl RegionAdmissionPolicy for WindowsRegionAdmission<'_> {
    type Request = AuthMultiMonitorRequestMsg;
    type Carrier = MultiMonitorCarrierMsg;
    type Plan = WindowsTopologyPlan;
    type Rejection = MultiMonitorDegradeReason;

    fn validate_advertised_offer(
        &self,
        requested: &AuthMultiMonitorRequestMsg,
    ) -> Result<(), MultiMonitorDegradeReason> {
        self.advertised()?
            .validate_request(requested)
            .map_err(MultiMonitorDegradeReason::ExceedsAdvertisedOffer)
    }

    fn select_carrier(
        &self,
        requested: &AuthMultiMonitorRequestMsg,
    ) -> Result<MultiMonitorCarrierMsg, MultiMonitorDegradeReason> {
        // This host has no carrier preference beyond the single carrier it
        // advertises, so `OFFERED_CARRIERS` doubles as the preferred order.
        self.advertised()?
            .select_carrier(requested, &OFFERED_CARRIERS)
            .map_err(MultiMonitorDegradeReason::ExceedsAdvertisedOffer)
    }

    fn convert_request(
        &self,
        requested: &AuthMultiMonitorRequestMsg,
    ) -> Result<RequestedMonitorTopology, MultiMonitorDegradeReason> {
        RequestedMonitorTopology::try_from(requested.requested_topology())
            .map_err(MultiMonitorDegradeReason::InvalidRequestedTopology)
    }

    fn plan_topology(
        &self,
        _requested: &AuthMultiMonitorRequestMsg,
        topology: &RequestedMonitorTopology,
        generation: TopologyGeneration,
    ) -> Result<WindowsTopologyPlan, MultiMonitorDegradeReason> {
        let Some(inventory) = self.inventory else {
            return Err(MultiMonitorDegradeReason::NoInventoryAvailable);
        };
        match self.planner {
            WindowsPlanner::Requested => {
                multi_monitor_topology::plan_topology(topology, generation, inventory)
            }
            WindowsPlanner::Current => {
                multi_monitor_topology::plan_current_topology(topology, generation, inventory)
            }
        }
        .map_err(MultiMonitorDegradeReason::PlanningFailed)
    }
}

/// Names this host's own degrade reason for every shared gate and every
/// host-typed refusal the shared admission driver can produce.
///
/// This is the whole of the translation between
/// [`arcen_outputs::admission`]'s host-independent outcome and this host's
/// public one: the gate order, the stage attribution, and the atomic-degrade
/// rule are the shared crate's, while the operator-facing wording stays here.
impl From<SharedAdmissionOutcome> for MultiMonitorOutcome {
    fn from(outcome: SharedAdmissionOutcome) -> Self {
        match outcome {
            AdmissionOutcome::NotRequested => Self::NotRequested,
            AdmissionOutcome::Degraded(DegradeReason::Gate(gate)) => Self::Degraded(match gate {
                GateClosed::CarrierNotReady => MultiMonitorDegradeReason::CarrierNotYetEnabled,
                GateClosed::OperatorDisabled => MultiMonitorDegradeReason::GateDisabled,
                GateClosed::NoInventoryAvailable => MultiMonitorDegradeReason::NoInventoryAvailable,
                GateClosed::NotAdvertised => MultiMonitorDegradeReason::NotAdvertised,
                // `GateClosed` is `#[non_exhaustive]`: a gate this host does
                // not yet name still refuses, and refusing is exactly what
                // `GateDisabled` already means to an operator. Fail closed
                // rather than admit an unknown gate's request.
                _ => MultiMonitorDegradeReason::GateDisabled,
            }),
            AdmissionOutcome::Degraded(DegradeReason::Rejected(rejection)) => {
                Self::Degraded(rejection.source)
            }
            AdmissionOutcome::Admitted { plan, carrier } => Self::Planned { plan, carrier },
        }
    }
}

/// The shared outcome this host's [`MultiMonitorOutcome`] is built from.
type SharedAdmissionOutcome =
    AdmissionOutcome<WindowsTopologyPlan, MultiMonitorCarrierMsg, MultiMonitorDegradeReason>;

/// Wraps a validated pre-auth offer in the minimal [`AuthRequest`] shape
/// needed to obtain the shared protocol crate's
/// [`AdvertisedMultiMonitorOffer`] evidence type (the only public
/// constructor for it), mirroring the exact `AuthRequest` this host already
/// sends to the client for this connection (see
/// `session::build_auth_request`) without re-deriving the shared crate's own
/// max-monitors/rotation/carrier validation here.
fn offer_wrapper(offer: &AuthMultiMonitorOfferMsg) -> AuthRequest {
    AuthRequest {
        msg_type: String::new(),
        auth_methods: Vec::new(),
        challenge: String::new(),
        salt: String::new(),
        auth_mode: None,
        disclaimer: None,
        multi_monitor_v1: Some(offer.clone()),
    }
}

#[derive(Debug)]
pub enum AppliedCapabilityError {
    MissingMediaPlan(String),
    InvalidClientDisplayId(String, ClientDisplayIdError),
    Invalid(MultiMonitorValidationError),
}

impl std::fmt::Display for AppliedCapabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMediaPlan(id) => {
                write!(
                    formatter,
                    "planned monitor {id:?} has no resolved media plan"
                )
            }
            Self::InvalidClientDisplayId(id, error) => {
                write!(
                    formatter,
                    "planned client display id {id:?} is invalid: {error}"
                )
            }
            Self::Invalid(error) => {
                write!(
                    formatter,
                    "applied multi_monitor_v1 capability is invalid: {error}"
                )
            }
        }
    }
}

impl std::error::Error for AppliedCapabilityError {}

impl From<MultiMonitorValidationError> for AppliedCapabilityError {
    fn from(error: MultiMonitorValidationError) -> Self {
        Self::Invalid(error)
    }
}

/// Builds this session's applied `server_hello.multi_monitor_v1` capability
/// from an admitted plan/carrier, every planned monitor's resolved media
/// plan, and the negotiated [`RegionMediaRoster`] its encoder set was
/// admitted with.
///
/// The join between planned regions, resolved media and negotiated plans —
/// its order, its all-or-nothing rule, and the rule that an applied region
/// publishes its negotiated budget verbatim — is
/// [`arcen_outputs::applied`]'s. This host keeps only the wire descriptor
/// shape, its own applied-desktop translation, and its error evidence.
///
/// # Errors
///
/// See [`AppliedCapabilityError`].
pub fn build_applied_capability(
    plan: &WindowsTopologyPlan,
    carrier: MultiMonitorCarrierMsg,
    media: &[(SessionMonitorId, ResolvedMediaPlan)],
    negotiated: &RegionMediaRoster,
) -> Result<ServerMultiMonitorMsg, AppliedCapabilityError> {
    // Unlike a dedicated server-side topology, Windows describes the physical
    // desktop as the interactive session already laid it out, so its origin
    // can legitimately be negative and the applied topology must publish the
    // shift that normalises it.
    let translation = OriginTranslation::to_origin(plan.desktop_x, plan.desktop_y);
    let descriptors = assemble_applied_regions(
        &WindowsAppliedRegions,
        &plan.monitors,
        media,
        negotiated,
        translation,
    )?;
    let topology = AppliedMonitorTopologyMsg::new(
        plan.generation.get(),
        translation
            .apply_x(plan.desktop_x)
            .map_err(|_| desktop_translation_overflow())?,
        translation
            .apply_y(plan.desktop_y)
            .map_err(|_| desktop_translation_overflow())?,
        plan.desktop_width,
        plan.desktop_height,
        translation.x(),
        translation.y(),
        carrier,
        descriptors,
    )?;
    Ok(ServerMultiMonitorMsg::new(
        u8::try_from(MAX_MULTI_MONITOR_COUNT).unwrap_or(4),
        RotationMsg::ALL.to_vec(),
        true,
        TopologyBackendKindMsg::PhysicalOutputs,
        OFFERED_CARRIERS.to_vec(),
        Some(topology),
    )?)
}

fn desktop_translation_overflow() -> AppliedCapabilityError {
    AppliedCapabilityError::Invalid(MultiMonitorValidationError::CoordinateOverflow(
        "Windows applied desktop translation",
    ))
}

fn monitor_translation_overflow() -> AppliedCapabilityError {
    AppliedCapabilityError::Invalid(MultiMonitorValidationError::CoordinateOverflow(
        "Windows applied monitor translation",
    ))
}

/// This host's implementation of the shared applied-region join.
///
/// Everything here is wire glue this crate owns: the protocol descriptor
/// shape, the `client_display_id` re-validation, this host's real
/// per-monitor `refresh_hz`, and its own coordinate-overflow evidence.
struct WindowsAppliedRegions;

impl AppliedRegionAssembler for WindowsAppliedRegions {
    type Region = WindowsMonitorPlan;
    type Resolved = ResolvedMediaPlan;
    type Descriptor = AppliedMonitorDescriptorMsg;
    type Error = AppliedCapabilityError;

    fn session_monitor_id(region: &WindowsMonitorPlan) -> SessionMonitorId {
        region.session_monitor_id
    }

    fn missing_media_plan(&self, region: &WindowsMonitorPlan) -> AppliedCapabilityError {
        AppliedCapabilityError::MissingMediaPlan(region.client_display_id.clone())
    }

    fn describe(
        &self,
        region: AppliedRegion<'_, WindowsMonitorPlan, ResolvedMediaPlan>,
    ) -> Result<AppliedMonitorDescriptorMsg, AppliedCapabilityError> {
        let monitor = region.region;
        let resolved = region.resolved;
        let client_display_id =
            ClientDisplayId::new(monitor.client_display_id.clone()).map_err(|error| {
                AppliedCapabilityError::InvalidClientDisplayId(
                    monitor.client_display_id.clone(),
                    error,
                )
            })?;
        Ok(AppliedMonitorDescriptorMsg {
            client_display_id,
            session_monitor_id: monitor.session_monitor_id.get(),
            x: region
                .translation
                .apply_x(monitor.x)
                .map_err(|_| monitor_translation_overflow())?,
            y: region
                .translation
                .apply_y(monitor.y)
                .map_err(|_| monitor_translation_overflow())?,
            width_px: monitor.width,
            height_px: monitor.height,
            refresh_hz: monitor.refresh_hz,
            rotation: monitor.rotation.into(),
            is_primary: monitor.primary,
            media_plan: AppliedMonitorMediaPlanMsg {
                stream_epoch: region.stream_epoch(),
                encoder_backend: resolved.backend.ready_token().to_owned(),
                encoder_class: resolved.backend.accelerator_class().token().to_owned(),
                codec: resolved.codec_token().to_owned(),
                chroma: resolved.chroma_token().to_owned(),
                width_px: resolved.width,
                height_px: resolved.height,
                fps: resolved.fps,
                // The host-authoritative per-region budget this session's
                // encoder set was actually admitted with, read verbatim off
                // the negotiated `RegionMediaPlan` rather than recomputed
                // here, so plan and wire cannot diverge.
                bitrate_kbps: region.bitrate_kbps(),
                cursor_mode: resolved.cursor_mode,
                degraded: false,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_monitor_topology::{AvailableOutput, OutputModeCapability};
    use crate::nvapi::AdapterLuid;

    const CROSS_HOST_OUTCOME_BASELINE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/runtime/multi_monitor_outcomes.json"
    ));

    fn descriptor(
        id: &str,
        client_monitor_id: u32,
        position: (i32, i32),
        size_px: (u32, u32),
        primary: bool,
        rotation: RotationMsg,
    ) -> RequestedMonitorDescriptorMsg {
        let (x, y) = position;
        let (width_px, height_px) = size_px;
        RequestedMonitorDescriptorMsg {
            client_display_id: arcen_protocol::messages::ClientDisplayId::new(id.to_owned())
                .expect("client display id"),
            client_monitor_id,
            x,
            y,
            width_px,
            height_px,
            logical_width: width_px,
            logical_height: height_px,
            scale: 1.0,
            refresh_hz: 60,
            rotation,
            is_primary: primary,
            name: format!("Display {id}"),
            width_mm: 0.0,
            height_mm: 0.0,
            vendor: 0,
            model: 0,
            serial: 0,
            edid: String::new(),
            safe_area_policy: arcen_protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
            quality_intent: arcen_protocol::messages::MonitorQualityIntentMsg::HostDefault,
        }
    }

    fn output(target_id: u32) -> AvailableOutput {
        AvailableOutput {
            adapter_luid: AdapterLuid {
                low_part: 1,
                high_part: 0,
            },
            target_id,
            adapter_output_index: target_id,
            adapter_name: "Test Adapter".to_owned(),
            global_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", target_id + 1),
            mode_capability: OutputModeCapability::CustomTimingCapable {
                min_width: 320,
                max_width: 7_680,
                min_height: 240,
                max_height: 4_320,
                min_refresh_hz: 30,
                max_refresh_hz: 240,
            },
            supported_rotations: Rotation::ALL.to_vec(),
            current_x: if target_id == 0 { 0 } else { 1_920 },
            current_y: 0,
            current_width: 1_920,
            current_height: 1_080,
            current_refresh_hz: 60,
            primary: target_id == 0,
        }
    }

    fn inventory(count: usize) -> PhysicalOutputInventory {
        PhysicalOutputInventory::new((0..count as u32).map(output).collect()).expect("inventory")
    }

    fn enabled_gate() -> MultiMonitorGate {
        MultiMonitorGate {
            advertise_enabled: true,
            max_monitors: None,
        }
    }

    /// Drives the shared admission steps for a request this host definitely
    /// received, in the pre-share call shape, so the gate/offer/carrier/
    /// planning assertions below stay byte-for-byte the ones that guarded the
    /// host-local implementation before it moved into
    /// [`arcen_outputs::admission`].
    ///
    /// `requested` is always `Some` here, so the driver's `NotRequested` arm
    /// is unreachable; the end-to-end `NotRequested` path is covered by the
    /// baseline outcome test.
    fn admit_request(
        gate: &MultiMonitorGate,
        inventory: Option<&PhysicalOutputInventory>,
        offer: Option<&AuthMultiMonitorOfferMsg>,
        requested: &AuthMultiMonitorRequestMsg,
    ) -> Result<(WindowsTopologyPlan, MultiMonitorCarrierMsg), MultiMonitorDegradeReason> {
        match admit_requested_topology(gate, inventory, offer, Some(requested)) {
            MultiMonitorOutcome::Planned { plan, carrier } => Ok((plan, carrier)),
            MultiMonitorOutcome::Degraded(reason) => Err(reason),
            MultiMonitorOutcome::NotRequested => {
                panic!("a request was supplied, so admission cannot be NotRequested")
            }
        }
    }

    fn request(
        monitors: Vec<RequestedMonitorDescriptorMsg>,
        carriers: Vec<MultiMonitorCarrierMsg>,
    ) -> AuthMultiMonitorRequestMsg {
        AuthMultiMonitorRequestMsg::new(
            RequestedMonitorTopologyMsg::new(monitors).expect("requested topology"),
            carriers,
        )
        .expect("auth request")
    }

    fn baseline_outcome_name(outcome: MultiMonitorOutcome) -> &'static str {
        match outcome {
            MultiMonitorOutcome::NotRequested => "not_requested",
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::GateDisabled) => {
                "gate_disabled"
            }
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::NoInventoryAvailable) => {
                "no_inventory"
            }
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::NotAdvertised) => {
                "not_advertised"
            }
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::ExceedsAdvertisedOffer(
                MultiMonitorValidationError::NoCommonAuthCarrier,
            )) => "no_common_carrier",
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::ExceedsAdvertisedOffer(_)) => {
                "exceeds_offer"
            }
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::PlanningFailed(_)) => {
                "planning_failed"
            }
            MultiMonitorOutcome::Planned {
                carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
                ..
            } => "planned_carrier_a",
            other => panic!("unexpected baseline outcome {other:?}"),
        }
    }

    #[test]
    fn cross_host_offer_degrade_and_carrier_outcomes_match_baseline() {
        let baseline: serde_json::Value =
            serde_json::from_str(CROSS_HOST_OUTCOME_BASELINE).unwrap();
        let offer = build_offer(&enabled_gate()).expect("offer");
        assert_eq!(
            u64::from(offer.max_monitors()),
            baseline["offer"]["max_monitors"].as_u64().unwrap()
        );
        assert_eq!(
            serde_json::to_value(offer.supported_rotations()).unwrap(),
            baseline["offer"]["supported_rotations"]
        );
        assert_eq!(
            serde_json::to_value(offer.carriers()).unwrap(),
            baseline["offer"]["carriers"]
        );

        let one_monitor = request(
            vec![descriptor(
                "primary",
                1,
                (0, 0),
                (1_920, 1_080),
                true,
                RotationMsg::Degrees0,
            )],
            OFFERED_CARRIERS.to_vec(),
        );
        let two_monitors = request(
            vec![
                descriptor(
                    "primary",
                    1,
                    (0, 0),
                    (1_920, 1_080),
                    true,
                    RotationMsg::Degrees0,
                ),
                descriptor(
                    "secondary",
                    2,
                    (1_920, 0),
                    (1_280, 720),
                    false,
                    RotationMsg::Degrees0,
                ),
            ],
            OFFERED_CARRIERS.to_vec(),
        );
        let no_common_carrier = request(
            vec![descriptor(
                "primary",
                1,
                (0, 0),
                (1_920, 1_080),
                true,
                RotationMsg::Degrees0,
            )],
            vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
        );
        let one_monitor_offer =
            AuthMultiMonitorOfferMsg::new(1, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .unwrap();
        let outcomes = [
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                Some(&inventory(4)),
                Some(&offer),
                None,
            )),
            baseline_outcome_name(admit_requested_topology(
                &MultiMonitorGate::disabled(),
                Some(&inventory(4)),
                Some(&offer),
                Some(&one_monitor),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                None,
                Some(&offer),
                Some(&one_monitor),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                Some(&inventory(4)),
                None,
                Some(&one_monitor),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                Some(&inventory(4)),
                Some(&one_monitor_offer),
                Some(&two_monitors),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                Some(&inventory(4)),
                Some(&offer),
                Some(&no_common_carrier),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                Some(&inventory(1)),
                Some(&offer),
                Some(&two_monitors),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(),
                Some(&inventory(2)),
                Some(&offer),
                Some(&two_monitors),
            )),
        ];
        assert_eq!(
            serde_json::to_value(outcomes).unwrap(),
            baseline["outcomes"]
        );
    }

    #[test]
    fn enabled_gate_advertises_the_muxed_carrier() {
        let offer = build_offer(&enabled_gate()).expect("offer");
        assert_eq!(offer.carriers(), OFFERED_CARRIERS);
    }

    #[test]
    fn offer_is_withheld_when_the_operator_gate_is_disabled() {
        assert!(build_offer(&MultiMonitorGate::disabled()).is_none());
    }

    #[test]
    fn preauth_offer_does_not_require_session_zero_display_inventory() {
        assert!(build_offer(&enabled_gate()).is_some());
    }

    #[test]
    fn unset_operator_ceiling_advertises_the_protocol_maximum() {
        let offer = build_offer(&enabled_gate()).expect("offer");
        assert_eq!(
            usize::from(offer.max_monitors()),
            MAX_MULTI_MONITOR_COUNT,
            "an operator who declared no ceiling keeps today's advertisement"
        );
    }

    /// Regression for `regress-comaintenance-multimon`: an operator running a
    /// host with a known smaller attached-output count must be able to make
    /// the pre-auth offer truthful, because this offer is built in session 0
    /// where the interactive desktop's outputs cannot be probed at all.
    #[test]
    fn operator_ceiling_caps_the_advertised_monitor_count() {
        let offer = build_offer(&MultiMonitorGate {
            advertise_enabled: true,
            max_monitors: Some(2),
        })
        .expect("offer");
        assert_eq!(offer.max_monitors(), 2);
        assert_eq!(offer.carriers(), OFFERED_CARRIERS);
    }

    #[test]
    fn native_nvidia_headless_offer_is_automatically_clamped_to_safe_capacity() {
        let config = WindowsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: None,
            nvidia_headless_enabled: true,
            ..WindowsMultiMonitorConfig::default()
        };
        let gate = MultiMonitorGate::from_config(&config);
        assert_eq!(
            build_offer(&gate).unwrap().max_monitors(),
            MAX_NVIDIA_HEADLESS_MONITORS
        );

        let lower = MultiMonitorGate::from_config(&WindowsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: Some(1),
            nvidia_headless_enabled: true,
            ..WindowsMultiMonitorConfig::default()
        });
        assert_eq!(build_offer(&lower).unwrap().max_monitors(), 1);

        let excessive = MultiMonitorGate::from_config(&WindowsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: Some(4),
            nvidia_headless_enabled: true,
            ..WindowsMultiMonitorConfig::default()
        });
        assert_eq!(
            build_offer(&excessive).unwrap().max_monitors(),
            MAX_NVIDIA_HEADLESS_MONITORS
        );
    }

    #[test]
    fn operator_ceiling_can_never_raise_the_protocol_maximum() {
        let offer = build_offer(&MultiMonitorGate {
            advertise_enabled: true,
            max_monitors: Some(u8::MAX),
        })
        .expect("offer");
        assert_eq!(usize::from(offer.max_monitors()), MAX_MULTI_MONITOR_COUNT);
    }

    #[test]
    fn operator_ceiling_never_reopens_a_disabled_gate() {
        assert!(build_offer(&MultiMonitorGate {
            advertise_enabled: false,
            max_monitors: Some(4),
        })
        .is_none());
    }

    /// A ceiling below what the client asked for is enforced by the shared
    /// offer validation the admission driver already runs, so a capped offer
    /// degrades rather than admitting a layout this host never advertised.
    #[test]
    fn admission_degrades_a_request_above_the_capped_offer() {
        let gate = MultiMonitorGate {
            advertise_enabled: true,
            max_monitors: Some(1),
        };
        let offer = build_offer(&gate).expect("offer");
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![
            descriptor("a", 1, (0, 0), (1_920, 1_080), true, RotationMsg::Degrees0),
            descriptor(
                "b",
                2,
                (1_920, 0),
                (1_920, 1_080),
                false,
                RotationMsg::Degrees0,
            ),
        ])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let outcome =
            admit_requested_topology(&gate, Some(&inventory(2)), Some(&offer), Some(&request));
        assert!(
            matches!(
                outcome,
                MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::ExceedsAdvertisedOffer(_))
            ),
            "unexpected outcome: {outcome:?}"
        );
    }

    #[test]
    fn admission_reports_not_requested_when_the_client_sends_nothing() {
        let outcome = admit_requested_topology(&enabled_gate(), Some(&inventory(2)), None, None);
        assert_eq!(outcome, MultiMonitorOutcome::NotRequested);
    }

    #[test]
    fn admission_plans_when_every_gate_is_open() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let outcome = admit_requested_topology(
            &enabled_gate(),
            Some(&inventory(2)),
            Some(&offer),
            Some(&request),
        );
        assert!(matches!(outcome, MultiMonitorOutcome::Planned { .. }));
    }

    #[test]
    fn admission_against_advertised_offer_plans_when_every_gate_is_open() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![
            descriptor("a", 1, (0, 0), (1_920, 1_080), true, RotationMsg::Degrees0),
            descriptor(
                "b",
                2,
                (1_920, 0),
                (1_920, 1_080),
                false,
                RotationMsg::Degrees0,
            ),
        ])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let (plan, carrier) =
            admit_request(&enabled_gate(), Some(&inventory(2)), Some(&offer), &request)
                .expect("planned");
        assert_eq!(plan.monitors.len(), 2);
        assert_eq!(carrier, MultiMonitorCarrierMsg::MuxedReliableStream);
    }

    #[test]
    fn admission_degrades_when_the_operator_gate_is_disabled() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let error = admit_request(
            &MultiMonitorGate::disabled(),
            Some(&inventory(2)),
            Some(&offer),
            &request,
        )
        .expect_err("degraded");
        assert_eq!(error, MultiMonitorDegradeReason::GateDisabled);
    }

    #[test]
    fn admission_degrades_when_no_inventory_is_available() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let error =
            admit_request(&enabled_gate(), None, Some(&offer), &request).expect_err("degraded");
        assert_eq!(error, MultiMonitorDegradeReason::NoInventoryAvailable);
    }

    #[test]
    fn admission_degrades_when_this_host_never_advertised() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let error = admit_request(&enabled_gate(), Some(&inventory(2)), None, &request)
            .expect_err("degraded");
        assert_eq!(error, MultiMonitorDegradeReason::NotAdvertised);
    }

    #[test]
    fn admission_degrades_when_the_request_exceeds_the_advertised_offer() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![
            descriptor("a", 1, (0, 0), (1_920, 1_080), true, RotationMsg::Degrees0),
            descriptor(
                "b",
                2,
                (1_920, 0),
                (1_920, 1_080),
                false,
                RotationMsg::Degrees0,
            ),
        ])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        // Offer advertises only 1 monitor max, but the request has 2.
        let offer =
            AuthMultiMonitorOfferMsg::new(1, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let error = admit_request(&enabled_gate(), Some(&inventory(2)), Some(&offer), &request)
            .expect_err("degraded");
        assert!(matches!(
            error,
            MultiMonitorDegradeReason::ExceedsAdvertisedOffer(_)
        ));
    }

    #[test]
    fn admission_degrades_with_a_carrier_intersection_failure_when_the_client_supports_none_in_common(
    ) {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )])
        .expect("requested topology");
        // Client only supports PerMonitorReliableStream; this host only
        // offers MuxedReliableStream — no common carrier.
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let error = admit_request(&enabled_gate(), Some(&inventory(2)), Some(&offer), &request)
            .expect_err("degraded");
        assert!(matches!(
            error,
            MultiMonitorDegradeReason::ExceedsAdvertisedOffer(
                MultiMonitorValidationError::NoCommonAuthCarrier
            )
        ));
    }

    #[test]
    fn admission_degrades_when_planning_fails_for_insufficient_outputs() {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![
            descriptor("a", 1, (0, 0), (1_920, 1_080), true, RotationMsg::Degrees0),
            descriptor(
                "b",
                2,
                (1_920, 0),
                (1_920, 1_080),
                false,
                RotationMsg::Degrees0,
            ),
        ])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        // Only one physical output is available, but two monitors are requested.
        let error = admit_request(&enabled_gate(), Some(&inventory(1)), Some(&offer), &request)
            .expect_err("degraded");
        assert!(matches!(
            error,
            MultiMonitorDegradeReason::PlanningFailed(WindowsTopologyError::InsufficientOutputs {
                requested: 2,
                available: 1,
            })
        ));
    }
    /// The negotiated roster this session's encoder set was admitted with,
    /// built the same way `encoder_admission` builds it for this exact plan
    /// (one region per planned monitor, at the planned geometry).
    fn negotiated_roster(plan: &WindowsTopologyPlan, budget_kbps: u32) -> RegionMediaRoster {
        let epoch =
            arcen_media::MediaStreamEpoch::new(plan.generation.get()).expect("nonzero epoch");
        RegionMediaRoster::new(
            plan.monitors
                .iter()
                .map(|monitor| {
                    arcen_media::RegionMediaPlan::new(
                        monitor.session_monitor_id,
                        epoch,
                        arcen_media::video::EncoderBackend::NativeNvenc,
                        arcen_media::VideoConfiguration::legacy_h264(),
                        monitor.width,
                        monitor.height,
                        60,
                        arcen_media::BitrateBudgetKbps::new(budget_kbps).expect("in-band budget"),
                    )
                    .expect("valid region media plan")
                })
                .collect(),
        )
        .expect("valid negotiated roster")
    }

    fn resolved_plan(width: u32, height: u32) -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend: arcen_media::video::EncoderBackend::NativeNvenc,
            video: arcen_media::VideoConfiguration::legacy_h264(),
            width,
            height,
            fps: 60,
            codecs: arcen_media::CodecSet::from_slice(&[arcen_media::VideoCodec::H264]),
            chroma: arcen_media::ChromaSet::from_slice(&[arcen_media::ChromaSubsampling::Yuv420]),
            bit_depths: arcen_media::BitDepthSet::from_slice(&[arcen_media::BitDepth::Eight]),
            ranges: arcen_media::ColorRangeSet::from_slice(&[arcen_media::ColorRange::Limited]),
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            cursor_in_video: false,
        }
    }

    fn planned_two_monitor_topology() -> (WindowsTopologyPlan, MultiMonitorCarrierMsg) {
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![
            descriptor("a", 1, (0, 0), (1_920, 1_080), true, RotationMsg::Degrees0),
            descriptor(
                "b",
                2,
                (1_920, 0),
                (1_920, 1_080),
                false,
                RotationMsg::Degrees0,
            ),
        ])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        admit_request(&enabled_gate(), Some(&inventory(2)), Some(&offer), &request)
            .expect("planned")
    }

    #[test]
    fn applied_media_plan_bitrate_is_the_negotiated_budget_not_the_applied_geometry() {
        // The proof that nothing recomputes a nominal budget at assembly
        // time: hand assembly a roster whose budgets are deliberately not the
        // nominal ones for the applied geometry and require the wire to carry
        // the negotiated numbers anyway.
        let (plan, carrier) = planned_two_monitor_topology();
        let media: Vec<(SessionMonitorId, ResolvedMediaPlan)> = plan
            .monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    resolved_plan(monitor.width, monitor.height),
                )
            })
            .collect();
        let budget = arcen_media::BitrateBudgetKbps::NOMINAL_FLOOR_KBPS;
        for monitor in &plan.monitors {
            assert_ne!(
                arcen_media::BitrateBudgetKbps::nominal_for_geometry(
                    monitor.width,
                    monitor.height,
                    60
                )
                .get(),
                budget,
                "the test's negotiated budget must actually differ from the nominal one"
            );
        }
        let negotiated = negotiated_roster(&plan, budget);

        let capability = build_applied_capability(&plan, carrier, &media, &negotiated)
            .expect("applied capability must build");
        let applied = capability
            .applied_topology()
            .expect("applied topology must be present");
        assert!(
            applied
                .monitors()
                .iter()
                .all(|monitor| monitor.media_plan.bitrate_kbps == budget),
            "the applied capability must publish the negotiated budget, never a re-derived one"
        );
        assert!(
            applied
                .monitors()
                .iter()
                .all(|monitor| monitor.media_plan.stream_epoch == plan.generation.get()),
            "every applied region carries the negotiated stream epoch"
        );
    }

    #[test]
    fn applied_capability_translates_a_negative_desktop_origin_to_a_non_negative_one() {
        let (mut plan, carrier) = planned_two_monitor_topology();
        // Re-place the roster the way Windows reports a secondary monitor
        // left of the primary: the desktop origin is negative and the applied
        // capability must publish the shift that normalises it.
        plan.desktop_x = -1_920;
        plan.monitors[1].x = -1_920;
        let media: Vec<(SessionMonitorId, ResolvedMediaPlan)> = plan
            .monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    resolved_plan(monitor.width, monitor.height),
                )
            })
            .collect();
        let negotiated =
            negotiated_roster(&plan, arcen_media::BitrateBudgetKbps::NOMINAL_FLOOR_KBPS);

        let capability = build_applied_capability(&plan, carrier, &media, &negotiated)
            .expect("applied capability must build");
        let applied = capability
            .applied_topology()
            .expect("applied topology must be present");
        assert_eq!(applied.translation_x(), 1_920);
        assert_eq!(applied.translation_y(), 0);
        assert_eq!(applied.desktop_x(), 0);
        assert_eq!(applied.monitors()[0].x, 1_920);
        assert_eq!(applied.monitors()[1].x, 0);
    }

    #[test]
    fn applied_capability_fails_when_a_planned_monitor_has_no_resolved_media() {
        let (plan, carrier) = planned_two_monitor_topology();
        let media = vec![(
            plan.monitors[0].session_monitor_id,
            resolved_plan(plan.monitors[0].width, plan.monitors[0].height),
        )];
        let negotiated =
            negotiated_roster(&plan, arcen_media::BitrateBudgetKbps::NOMINAL_FLOOR_KBPS);

        let error = build_applied_capability(&plan, carrier, &media, &negotiated)
            .expect_err("must fail when a monitor's media plan is missing");
        assert!(matches!(error, AppliedCapabilityError::MissingMediaPlan(_)));
    }

    #[test]
    fn both_admission_entry_points_run_the_same_gate_sequence() {
        // `admit_requested_current_topology` differs from
        // `admit_requested_topology` only in which planner it hands the
        // converted topology to; everything before planning is one shared
        // sequence, so every gate/offer/carrier refusal must be identical.
        let requested_topology = RequestedMonitorTopologyMsg::new(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )])
        .expect("requested topology");
        let request = AuthMultiMonitorRequestMsg::new(
            requested_topology,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request");
        let offer =
            AuthMultiMonitorOfferMsg::new(4, RotationMsg::ALL.to_vec(), OFFERED_CARRIERS.to_vec())
                .expect("offer");
        let inventory = inventory(2);
        let cases: [(
            &MultiMonitorGate,
            Option<&PhysicalOutputInventory>,
            Option<&AuthMultiMonitorOfferMsg>,
            Option<&AuthMultiMonitorRequestMsg>,
        ); 5] = [
            (&MultiMonitorGate::disabled(), None, None, None),
            (
                &MultiMonitorGate {
                    advertise_enabled: false,
                    max_monitors: None,
                },
                Some(&inventory),
                Some(&offer),
                Some(&request),
            ),
            (&enabled_gate(), None, Some(&offer), Some(&request)),
            (&enabled_gate(), Some(&inventory), None, Some(&request)),
            (
                &enabled_gate(),
                Some(&inventory),
                Some(&offer),
                Some(&request),
            ),
        ];
        for (gate, inventory, offer, requested) in cases {
            assert_eq!(
                admit_requested_topology(gate, inventory, offer, requested),
                admit_requested_current_topology(gate, inventory, offer, requested),
                "both entry points must refuse or admit identically before planning"
            );
        }
    }
    /// The exact interactive-session output inventory probed on the
    /// `pier-windows-software.example.internal` Windows host: a QEMU guest whose only display device
    /// is std-VGA behind the inbox Microsoft Basic Display Adapter. One
    /// output, fixed 64 Hz modes, no NVIDIA driver and no NVENC.
    fn co_maintenance_inventory() -> PhysicalOutputInventory {
        PhysicalOutputInventory::new(vec![AvailableOutput {
            adapter_luid: AdapterLuid {
                low_part: 0x0000_66d5,
                high_part: 0,
            },
            target_id: 0,
            adapter_output_index: 0,
            adapter_name: "Microsoft Basic Render Driver".to_owned(),
            global_index: 0,
            device_name: r"\\.\DISPLAY1".to_owned(),
            mode_capability: OutputModeCapability::FixedModes(vec![
                multi_monitor_topology::OutputMode {
                    width: 1_920,
                    height: 1_200,
                    refresh_hz: 64,
                },
            ]),
            supported_rotations: vec![Rotation::Degrees0],
            current_x: 0,
            current_y: 0,
            current_width: 1_920,
            current_height: 1_200,
            current_refresh_hz: 64,
            primary: true,
        }])
        .expect("inventory")
    }

    fn co_maintenance_request(monitors: usize) -> AuthMultiMonitorRequestMsg {
        let descriptors = (0..monitors)
            .map(|index| {
                let mut descriptor = descriptor(
                    &format!("deck-{}", index + 1),
                    u32::try_from(index + 1).expect("monitor id"),
                    (i32::try_from(index).expect("index") * 1_920, 0),
                    (1_920, 1_200),
                    index == 0,
                    RotationMsg::Degrees0,
                );
                descriptor.refresh_hz = 64;
                descriptor
            })
            .collect::<Vec<_>>();
        AuthMultiMonitorRequestMsg::new(
            RequestedMonitorTopologyMsg::new(descriptors).expect("requested topology"),
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("auth request")
    }

    /// Regression for `regress-comaintenance-multimon`, end to end through
    /// the production functions on the exact inventory probed inside the
    /// interactive session of the real software/OpenH264 host: the operator gate
    /// produces a truthful one-monitor offer, the shared admission driver
    /// admits a matching one-monitor Match-My-Layout request, and the
    /// resulting plan is served by an all-software OpenH264 encoder
    /// set with no NVENC and no synthesized timing anywhere.
    #[test]
    fn software_host_negotiates_and_admits_a_truthful_one_monitor_layout() {
        let gate = MultiMonitorGate {
            advertise_enabled: true,
            max_monitors: Some(1),
        };
        let offer = build_offer(&gate).expect("offer");
        assert_eq!(offer.max_monitors(), 1);

        let inventory = co_maintenance_inventory();
        let outcome = admit_requested_topology(
            &gate,
            Some(&inventory),
            Some(&offer),
            Some(&co_maintenance_request(1)),
        );
        let MultiMonitorOutcome::Planned { plan, carrier } = outcome else {
            panic!("expected a planned topology, got {outcome:?}");
        };
        assert_eq!(carrier, MultiMonitorCarrierMsg::MuxedReliableStream);
        assert_eq!(plan.monitors.len(), 1);
        assert!(!plan.requires_custom_timing);
        assert_eq!(
            plan.monitors[0].adapter_name,
            "Microsoft Basic Render Driver"
        );

        let template = crate::multi_monitor_capenc::MonitorPipelineTemplate {
            codec: arcen_protocol::VideoCodec::H264,
            chroma: arcen_protocol::ChromaSubsampling::Yuv420,
            bit_depth: arcen_media::BitDepth::Eight,
            color_range: arcen_media::ColorRange::Limited,
            color_matrix: arcen_media::ColorMatrix::Bt709,
            intent: arcen_media::EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 60,
            encoder: Some(crate::capenc::EncoderSelection::SoftwareH264),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            session_log_id: arcen_telemetry::CorrelationId::from_uuid_v4_bytes([0; 16]),
        };
        let encoder_plan = crate::encoder_admission::plan_encoder_sets(
            &plan,
            &template,
            &std::collections::BTreeMap::new(),
            &["Microsoft Basic Render Driver".to_owned()],
            None,
            false,
        )
        .expect("software encoder set");
        assert_eq!(encoder_plan.candidate_count(), 1);
    }

    /// The same host, same gate: a two-monitor Match-My-Layout request is
    /// refused by the advertised offer itself, so this host never plans a
    /// layout its single physical output cannot serve.
    #[test]
    fn software_host_refuses_a_two_monitor_layout_it_cannot_serve() {
        let gate = MultiMonitorGate {
            advertise_enabled: true,
            max_monitors: Some(1),
        };
        let offer = build_offer(&gate).expect("offer");
        let outcome = admit_requested_topology(
            &gate,
            Some(&co_maintenance_inventory()),
            Some(&offer),
            Some(&co_maintenance_request(2)),
        );
        assert!(
            matches!(
                outcome,
                MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::ExceedsAdvertisedOffer(_))
            ),
            "unexpected outcome: {outcome:?}"
        );
    }

    /// And with the ceiling left unset (today's default advertisement), the
    /// refusal moves one stage later but is still atomic: planning fails on
    /// the physical output count rather than serving a subset.
    #[test]
    fn software_host_refuses_a_two_monitor_layout_at_planning_without_a_ceiling() {
        let gate = enabled_gate();
        let offer = build_offer(&gate).expect("offer");
        let outcome = admit_requested_topology(
            &gate,
            Some(&co_maintenance_inventory()),
            Some(&offer),
            Some(&co_maintenance_request(2)),
        );
        assert!(
            matches!(
                outcome,
                MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::PlanningFailed(
                    WindowsTopologyError::InsufficientOutputs {
                        requested: 2,
                        available: 1,
                    }
                ))
            ),
            "unexpected outcome: {outcome:?}"
        );
    }
}
