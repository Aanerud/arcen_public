//! Pre-auth `multi_monitor_v1` capability offer and post-auth admission
//! gating.
//!
//! This module is the only place that decides whether this host advertises
//! `multi_monitor_v1` before authentication and whether an authenticated
//! client's requested topology is admitted. It never starts an X server or a
//! capenc child; it only produces a typed decision that `net::server` logs
//! and, once the decision is [`MultiMonitorOutcome::Planned`], acts on by
//! spawning a per-monitor capenc supervisor and a
//! session-launcher-generated multi-head dedicated Xorg.
//!
//! Two independent gates must both be open before this host ever advertises
//! or admits `multi_monitor_v1`:
//!
//! - **Operator gate** ([`MultiMonitorGate::advertise_enabled`] plus a
//!   non-empty configured head inventory): the explicit config/CLI opt-in
//!   this tranche's task requires.
//! - **Carrier gate** ([`crate::media::multi_capenc::MULTI_MONITOR_CARRIER_READY`]):
//!   a hardcoded `true` constant now that the capenc supervisor, generated
//!   multi-head Xorg config, RandR verification, and `MonitorMux` transport
//!   wiring are all complete — the operator gate above is the sole
//!   remaining production safety switch.
//!
//! Legacy/default behavior (no offer, no request, existing single-primary /
//! `match_layout` degrade in `session::auth`) is completely unaffected: this
//! module only adds a new, additive decision path that a caller must opt
//! into, and it can never make session establishment less safe than today,
//! only refuse to enable the new one.

use arcen_media::video::ResolvedMediaPlan;
use arcen_media::{
    MediaContractError, RegionMediaRoster, RequestedMonitorTopology, SessionMonitorId,
    TopologyGeneration,
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
    MultiMonitorCarrierMsg, MultiMonitorValidationError, RotationMsg, ServerMultiMonitorMsg,
    TopologyBackendKindMsg,
};
use thiserror::Error;

use crate::config::LinuxMultiMonitorConfig;
use crate::display::topology::{
    self, HeadCapability, HeadInventory, LinuxMonitorPlan, LinuxTopologyError, LinuxTopologyPlan,
};
use crate::media::capenc::EncoderSelection;
use crate::media::multi_capenc::{
    validate_uniform_exact_encoder_policy, MultiCapencConfigError, MULTI_MONITOR_CARRIER_READY,
};

/// Reliable carrier this Linux tranche advertises once its carrier gate
/// opens: `MuxedReliableStream` (Carrier A) multiplexes every applied
/// monitor's tagged `VideoHeader` frames onto the session's one existing
/// reliable transport stream via `session::monitor_mux::MonitorMux`. This is
/// independent of the capenc *process* model: `media::multi_capenc` still
/// runs one dedicated `capenc` child per monitor for capture isolation, but
/// their encoded output is multiplexed at the wire-frame level onto a single
/// stream rather than opened as one transport stream per monitor
/// (`PerMonitorReliableStream`, Carrier B), which this Linux tranche does
/// not select.
const OFFERED_CARRIERS: [MultiMonitorCarrierMsg; 1] = [MultiMonitorCarrierMsg::MuxedReliableStream];

/// Explicit operator-facing gate for `multi_monitor_v1`. Defaults to fully
/// disabled; both `advertise_enabled` and a usable `inventory` must be
/// explicitly set by configuration for this host to ever advertise
/// multi-monitor support. `encoder` carries this host's configured
/// `video.encoder` policy so [`build_offer`] and
/// [`admit_requested_topology`] can apply the same fail-closed
/// encoder-admission contract `media::multi_capenc` already enforces later
/// (post-Xorg-commit, pre-spawn) — but earlier still, before this host ever
/// advertises the capability or commits a dedicated Xorg for an admitted
/// request.
#[derive(Debug, Clone, Default)]
pub struct MultiMonitorGate {
    /// Explicit config/CLI opt-in. `false` unless an operator turns it on.
    pub advertise_enabled: bool,
    /// Configured/discovered NVIDIA head inventory. `None`/empty means no
    /// heads are available to plan against regardless of `advertise_enabled`.
    pub inventory: Option<HeadInventory>,
    /// This host's configured `video.encoder` policy. `Auto` (the type
    /// default) and `WindowsMediaFoundation` withhold the offer outright
    /// ([`build_offer`]); `NativeNvenc` and `SoftwareH264` are otherwise
    /// admitted, with `SoftwareH264` additionally re-checked against the
    /// planned topology's exact geometry at admission time (see
    /// [`LinuxRegionAdmission::check_media_policy`]).
    pub encoder: EncoderSelection,
}

impl MultiMonitorGate {
    /// The fully disabled gate (today's legacy/default behavior).
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            advertise_enabled: false,
            inventory: None,
            encoder: EncoderSelection::Auto,
        }
    }

    /// Builds an operator gate from the validated `[platform.multi_monitor]`
    /// config section and this host's configured `video.encoder` policy.
    ///
    /// Every configured head is treated as rotation-capable
    /// ([`HeadCapability::new`]): this tranche has no per-head rotation
    /// discovery yet (pending pier-linux.example.internal hardware validation, see
    /// `hosts/linux/AGENTS.md`), so a conservative "assume full rotation
    /// support" default is used rather than inventing an unvalidated
    /// per-head config schema this tranche cannot yet back with real
    /// discovery.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`LinuxTopologyError`] when advertisement is
    /// enabled but the configured head roster itself is invalid (duplicate
    /// or unrecognized head token), so a caller can log a clear
    /// configuration diagnostic and fall back to the safe disabled gate
    /// rather than silently accepting a broken roster. In practice this
    /// should already be unreachable in production: `cli::parse` now builds
    /// and rejects an invalid `HeadInventory` at config-load/CLI-parse time
    /// (`validate-config` fails closed), so this constructor should only
    /// ever see an already-valid roster; it is kept fallible regardless,
    /// since this is the only place that actually builds the inventory this
    /// gate carries.
    pub fn from_config(
        config: &LinuxMultiMonitorConfig,
        encoder: EncoderSelection,
    ) -> Result<Self, LinuxTopologyError> {
        if !config.advertise_enabled {
            return Ok(Self::disabled());
        }
        if config.heads.is_empty() {
            return Ok(Self {
                advertise_enabled: true,
                inventory: None,
                encoder,
            });
        }
        let heads = config
            .heads
            .iter()
            .cloned()
            .map(HeadCapability::new)
            .collect();
        let inventory = HeadInventory::new(heads)?;
        Ok(Self {
            advertise_enabled: true,
            inventory: Some(inventory),
            encoder,
        })
    }

    fn usable_inventory(&self) -> Option<&HeadInventory> {
        self.inventory
            .as_ref()
            .filter(|inventory| !inventory.is_empty())
    }
}

/// Outcome of admitting an authenticated client's requested `multi_monitor_v1`
/// topology (if any) against this host's gate and head inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiMonitorOutcome {
    /// The client did not send a `multi_monitor_v1` requested topology;
    /// today's legacy/default single-primary (or windowed / `match_layout`)
    /// behavior in `session::auth` applies completely unchanged.
    NotRequested,
    /// The client requested multi-monitor but this host falls back to its
    /// existing single-primary degrade behavior for a documented, typed
    /// reason. No partial topology is ever applied: admission is atomic —
    /// either every requested monitor plans successfully, or none do.
    Degraded(MultiMonitorDegradeReason),
    /// The requested topology was fully, atomically planned onto this host's
    /// configured head inventory, using `carrier` — the host-selected
    /// element of the client/host common carrier intersection computed via
    /// [`AdvertisedMultiMonitorOffer::select_carrier`] — as the carrier this
    /// session applies. Reaching this variant already required the carrier
    /// gate to be open, so `net::server` acts on it: spawning a per-monitor
    /// capenc supervisor, a session-launcher-generated multi-head dedicated
    /// Xorg, and the `MonitorMux`-based transport this `carrier` names.
    Planned {
        plan: LinuxTopologyPlan,
        carrier: MultiMonitorCarrierMsg,
    },
}

impl MultiMonitorOutcome {
    /// Extracts this outcome's committed plan/carrier pair, or `None` for
    /// every non-[`Self::Planned`] outcome (`NotRequested`, or `Degraded` for
    /// any reason — including the operator's own gate staying off, this
    /// host's default).
    ///
    /// Consumed by `net::server`'s session-creation path to thread a
    /// [`Self::Planned`] outcome's plan/carrier into
    /// [`crate::session::lifecycle::SessionRegistry::acquire`], so it can be
    /// committed once at `Create` time and persist unchanged across
    /// reconnects (a reconnecting attempt's freshly recomputed outcome is
    /// discarded in favor of the original `Create`'s stored plan).
    #[must_use]
    pub fn into_planned(self) -> Option<(LinuxTopologyPlan, MultiMonitorCarrierMsg)> {
        match self {
            Self::Planned { plan, carrier } => Some((plan, carrier)),
            Self::NotRequested | Self::Degraded(_) => None,
        }
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
                GateClosed::NoInventoryAvailable => {
                    MultiMonitorDegradeReason::NoInventoryConfigured
                }
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
    AdmissionOutcome<LinuxTopologyPlan, MultiMonitorCarrierMsg, MultiMonitorDegradeReason>;

/// Typed reason this host degraded a requested `multi_monitor_v1` topology to
/// the existing single-primary behavior instead of admitting it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MultiMonitorDegradeReason {
    #[error("the capenc multi-monitor carrier is not yet enabled on this host")]
    CarrierNotYetEnabled,
    #[error("the multi_monitor_v1 advertisement gate is disabled on this host")]
    GateDisabled,
    #[error("no NVIDIA head inventory is configured for multi_monitor_v1 on this host")]
    NoInventoryConfigured,
    #[error("client requested multi_monitor_v1 without this host having advertised it")]
    NotAdvertised,
    #[error("requested topology exceeds this host's advertised multi_monitor_v1 offer: {0}")]
    ExceedsAdvertisedOffer(MultiMonitorValidationError),
    #[error("requested topology is invalid: {0}")]
    InvalidRequestedTopology(MediaContractError),
    #[error("requested topology could not be planned onto the configured head inventory: {0}")]
    PlanningFailed(LinuxTopologyError),
    #[error(
        "requested topology's committed geometry is not exactly supported by this host's \
         configured encoder policy: {0}"
    )]
    EncoderPolicyRejected(MultiCapencConfigError),
}

/// The `(max_monitors, supported_rotations)` this host declares for a given
/// head inventory, shared by [`build_offer`]'s pre-auth advertisement and
/// [`build_applied_capability`]'s post-admission applied capability so the
/// applied capability can never advertise more than this session's original
/// pre-auth offer already promised.
///
/// Every configured head is currently assumed rotation-capable (see
/// [`MultiMonitorGate::from_config`]'s doc comment), so `supported_rotations`
/// only narrows to `[Degrees0]` when at least one configured head is
/// declared non-rotation-capable.
fn advertised_capability(inventory: &HeadInventory) -> Option<(u8, Vec<RotationMsg>)> {
    let max_monitors = u8::try_from(inventory.len().min(topology::VALID_HEAD_TOKENS.len())).ok()?;
    let supported_rotations = if inventory.heads().iter().all(|head| head.supports_rotation) {
        RotationMsg::ALL.to_vec()
    } else {
        vec![RotationMsg::Degrees0]
    };
    Some((max_monitors, supported_rotations))
}

/// Builds this host's pre-auth `multi_monitor_v1` offer for `AuthRequest`, or
/// `None` when any gate is closed: operator gate disabled/no heads
/// configured, the carrier gate is not yet open, or the configured
/// `video.encoder` policy is not one this offer can ever honor
/// (`Auto`/`WindowsMediaFoundation` — see [`MultiMonitorGate::encoder`]).
/// Withholding the offer itself (rather than only rejecting admission later)
/// means an incompatible-encoder host never advertises a capability it
/// cannot deliver, so a client never even attempts to request it.
#[must_use]
pub fn build_offer(gate: &MultiMonitorGate) -> Option<AuthMultiMonitorOfferMsg> {
    if !MULTI_MONITOR_CARRIER_READY || !gate.advertise_enabled {
        return None;
    }
    if matches!(
        gate.encoder,
        EncoderSelection::Auto | EncoderSelection::WindowsMediaFoundation
    ) {
        return None;
    }
    let inventory = gate.usable_inventory()?;
    let (max_monitors, supported_rotations) = advertised_capability(inventory)?;
    AuthMultiMonitorOfferMsg::new(max_monitors, supported_rotations, OFFERED_CARRIERS.to_vec()).ok()
}

/// Admits (or degrades) an authenticated client's optional requested
/// `multi_monitor_v1` topology.
///
/// `offer` must be the exact offer this host attached to the `AuthRequest`
/// that preceded `requested` (or `None` if this host never advertised
/// support this connection). `requested` is the client's `AuthResponse`
/// `multi_monitor_v1` sidecar: the requested topology plus the client's
/// ordered carrier support, bundled together since [`AuthMultiMonitorRequestMsg`]
/// became the auth-time carrier authority.
///
/// The gate order itself lives in [`arcen_outputs::admission::admit_regions`],
/// not here: this function only states which of this host's facts open each
/// shared gate and hands over a [`LinuxRegionAdmission`] policy for the
/// host-shaped steps. The carrier gate is checked first and unconditionally,
/// so every other check (operator gate, inventory,
/// advertised-offer/carrier validation, request conversion, planning, encoder
/// policy) only ever runs once [`MULTI_MONITOR_CARRIER_READY`] is `true` —
/// now always the case, so this host's admitted behavior is governed entirely
/// by `gate` (the operator's own explicit, default-off configuration) and what
/// the client requested.
#[must_use]
pub fn admit_requested_topology(
    gate: &MultiMonitorGate,
    offer: Option<&AuthMultiMonitorOfferMsg>,
    requested: Option<&AuthMultiMonitorRequestMsg>,
) -> MultiMonitorOutcome {
    let policy = LinuxRegionAdmission::new(gate, offer);
    admit_regions(policy.gates(), &policy, requested).into()
}

/// This host's implementation of the shared multi-region admission steps.
///
/// Holds the `AuthRequest` wrapper for this connection's offer so the
/// advertised-offer evidence is derived once, then borrowed by both the offer
/// and carrier steps.
struct LinuxRegionAdmission<'a> {
    gate: &'a MultiMonitorGate,
    inventory: Option<&'a HeadInventory>,
    /// `Some` exactly when this host advertised an offer on this connection.
    offer: Option<AuthRequest>,
}

impl<'a> LinuxRegionAdmission<'a> {
    fn new(gate: &'a MultiMonitorGate, offer: Option<&AuthMultiMonitorOfferMsg>) -> Self {
        Self {
            gate,
            inventory: gate.usable_inventory(),
            offer: offer.map(offer_wrapper),
        }
    }

    fn gates(&self) -> AdmissionGates {
        AdmissionGates {
            carrier_ready: MULTI_MONITOR_CARRIER_READY,
            operator_enabled: self.gate.advertise_enabled,
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

impl RegionAdmissionPolicy for LinuxRegionAdmission<'_> {
    type Request = AuthMultiMonitorRequestMsg;
    type Carrier = MultiMonitorCarrierMsg;
    type Plan = LinuxTopologyPlan;
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
        requested: &AuthMultiMonitorRequestMsg,
        topology: &RequestedMonitorTopology,
        generation: TopologyGeneration,
    ) -> Result<LinuxTopologyPlan, MultiMonitorDegradeReason> {
        let Some(inventory) = self.inventory else {
            return Err(MultiMonitorDegradeReason::NoInventoryConfigured);
        };
        let mut plan = topology::plan_topology(topology, generation, inventory)
            .map_err(MultiMonitorDegradeReason::PlanningFailed)?;
        for (planned, requested) in plan
            .monitors
            .iter_mut()
            .zip(requested.requested_topology().monitors().iter())
        {
            planned.quality_intent = requested.quality_intent;
        }
        Ok(plan)
    }

    /// Defense-in-depth, run unconditionally after planning: [`build_offer`]
    /// already withholds the offer for `Auto`/`WindowsMediaFoundation`, so an
    /// honest client can never even reach this admission path with an
    /// incompatible encoder — but this host's own committed geometry is only
    /// known once `plan_topology` has resolved every monitor's exact applied
    /// size, so the "would `SoftwareH264` clamp any monitor" half of the
    /// fail-closed contract can only be checked here, not earlier. Refusing
    /// at this step guarantees no dedicated Xorg for this topology is ever
    /// committed for a policy-incompatible plan.
    fn check_media_policy(
        &self,
        plan: &LinuxTopologyPlan,
    ) -> Result<(), MultiMonitorDegradeReason> {
        validate_uniform_exact_encoder_policy(plan, self.gate.encoder)
            .map_err(MultiMonitorDegradeReason::EncoderPolicyRejected)
    }
}

/// Wraps a validated pre-auth offer in the minimal [`AuthRequest`] shape
/// needed to obtain the shared protocol crate's
/// [`AdvertisedMultiMonitorOffer`] evidence type (the only public
/// constructor for it), mirroring the exact `AuthRequest` this host already
/// sent to the client for this connection (see
/// `net::server::build_auth_request`) without re-deriving the shared
/// crate's own max-monitors/rotation/carrier validation here.
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

/// Failure building the applied `server_hello.multi_monitor_v1` capability
/// from a [`MultiMonitorOutcome::Planned`] plan (see
/// [`build_applied_capability`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AppliedCapabilityError {
    /// The operator gate's head inventory has become unusable since the
    /// request was admitted. Should be unreachable in practice — reaching
    /// [`MultiMonitorOutcome::Planned`] already required a usable inventory
    /// — but kept as a typed guard rather than an `unreachable!()` in case
    /// that invariant is ever weakened upstream.
    #[error("no NVIDIA head inventory is configured for multi_monitor_v1 on this host")]
    NoInventoryConfigured,
    /// A planned monitor has no matching resolved `capenc` media plan.
    /// Every planned monitor's dedicated capenc child must have started and
    /// resolved successfully (see
    /// `media::multi_capenc::MultiCapencSupervisor::take_frame_sources`)
    /// before this is called.
    #[error(
        "planned monitor {0:?} has no resolved media plan; its capenc child must start before the applied capability can be built"
    )]
    MissingMediaPlan(String),
    /// A planned monitor's stored `client_display_id` is no longer a valid
    /// [`ClientDisplayId`]. Should be unreachable — the id already passed
    /// this exact validation once when the client's requested topology was
    /// admitted (see `RequestedMonitor::try_from`) and planning only copies
    /// it verbatim — but kept as a typed guard for the same reason as
    /// [`AppliedCapabilityError::NoInventoryConfigured`].
    #[error("planned monitor client display id {0:?} is invalid: {1}")]
    InvalidClientDisplayId(String, ClientDisplayIdError),
    /// A planned monitor's applied origin left the published coordinate
    /// range. Unreachable on this host — `display::topology::plan_topology`
    /// always lands the applied desktop on a non-negative origin, so the
    /// shared translation is the identity — but kept as a typed guard rather
    /// than an `unreachable!()`.
    #[error("applied multi_monitor_v1 origin translation left the published coordinate range")]
    AppliedTranslationOverflow,
    /// The assembled capability failed the shared protocol crate's own
    /// `multi_monitor_v1` wire invariants.
    #[error("applied multi_monitor_v1 capability is invalid: {0}")]
    Invalid(#[from] MultiMonitorValidationError),
}

/// Builds this session's applied `server_hello.multi_monitor_v1` capability
/// from a [`MultiMonitorOutcome::Planned`] plan/carrier, every planned
/// monitor's resolved `capenc` media plan, and the negotiated
/// [`RegionMediaRoster`] its encoder set was admitted with, for
/// `session::handshake::build_server_hello` to attach before this session's
/// first IDR is requested.
///
/// `media` must contain one `(session_monitor_id, ResolvedMediaPlan)` entry
/// per monitor in `plan.monitors`, and `negotiated` one plan per monitor,
/// both matched by [`SessionMonitorId`] rather than by roster position or
/// NvFBC output index — a `MultiCapencSupervisor`'s child/routing order is
/// not guaranteed to match `plan.monitors`'s requested-roster order. The join
/// itself, its order, and the "publish the negotiated budget verbatim" rule
/// are [`arcen_outputs::applied`]'s.
///
/// # Errors
///
/// See [`AppliedCapabilityError`].
pub fn build_applied_capability(
    gate: &MultiMonitorGate,
    plan: &LinuxTopologyPlan,
    carrier: MultiMonitorCarrierMsg,
    media: &[(SessionMonitorId, ResolvedMediaPlan)],
    negotiated: &RegionMediaRoster,
) -> Result<ServerMultiMonitorMsg, AppliedCapabilityError> {
    let inventory = gate
        .usable_inventory()
        .ok_or(AppliedCapabilityError::NoInventoryConfigured)?;
    let (max_monitors, supported_rotations) =
        advertised_capability(inventory).ok_or(AppliedCapabilityError::NoInventoryConfigured)?;

    // `display::topology::plan_topology` always translates the applied
    // bounding box's minimum origin to exactly `(0, 0)`
    // (`LayoutBounds::translation_to_origin`'s definition only shifts when
    // `x<0`/`y<0`, and the primary monitor's own rect is always placed at
    // `(0, 0)` by construction), so this host's applied desktop always starts
    // at a non-negative origin and the shared translation is the identity.
    let translation = OriginTranslation::to_origin(0, 0);
    let descriptors = assemble_applied_regions(
        &LinuxAppliedRegions,
        &plan.monitors,
        media,
        negotiated,
        translation,
    )?;

    let applied_topology = AppliedMonitorTopologyMsg::new(
        plan.generation.get(),
        0,
        0,
        plan.virtual_width,
        plan.virtual_height,
        translation.x(),
        translation.y(),
        carrier,
        descriptors,
    )?;

    Ok(ServerMultiMonitorMsg::new(
        max_monitors,
        supported_rotations,
        true,
        TopologyBackendKindMsg::DedicatedXorg,
        OFFERED_CARRIERS.to_vec(),
        Some(applied_topology),
    )?)
}

/// This host's implementation of the shared applied-region join.
///
/// Everything here is wire glue this crate owns: the protocol descriptor
/// shape, the `client_display_id` re-validation, and this host's own
/// `refresh_hz` placeholder. The join order and the budget rule are
/// [`assemble_applied_regions`]'s.
struct LinuxAppliedRegions;

impl AppliedRegionAssembler for LinuxAppliedRegions {
    type Region = LinuxMonitorPlan;
    type Resolved = ResolvedMediaPlan;
    type Descriptor = AppliedMonitorDescriptorMsg;
    type Error = AppliedCapabilityError;

    fn session_monitor_id(region: &LinuxMonitorPlan) -> SessionMonitorId {
        region.session_monitor_id
    }

    fn missing_media_plan(&self, region: &LinuxMonitorPlan) -> AppliedCapabilityError {
        AppliedCapabilityError::MissingMediaPlan(region.client_display_id.clone())
    }

    fn describe(
        &self,
        region: AppliedRegion<'_, LinuxMonitorPlan, ResolvedMediaPlan>,
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
                .map_err(|_| AppliedCapabilityError::AppliedTranslationOverflow)?,
            y: region
                .translation
                .apply_y(monitor.y)
                .map_err(|_| AppliedCapabilityError::AppliedTranslationOverflow)?,
            width_px: monitor.width,
            height_px: monitor.height,
            // Not tracked through `LinuxTopologyPlan`/`LinuxMonitorPlan` in
            // this tranche (the client's originally requested refresh rate
            // is discarded during planning); documented placeholder.
            // Diagnostic only — no invariant depends on this value.
            refresh_hz: 60,
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
    use arcen_media::Rotation;
    use arcen_protocol::messages::{RequestedMonitorDescriptorMsg, RequestedMonitorTopologyMsg};

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

    fn enabled_gate(heads: usize) -> MultiMonitorGate {
        enabled_gate_with_encoder(heads, EncoderSelection::NativeNvenc)
    }

    fn enabled_gate_with_encoder(heads: usize, encoder: EncoderSelection) -> MultiMonitorGate {
        MultiMonitorGate {
            advertise_enabled: true,
            inventory: Some(
                HeadInventory::new(
                    topology::VALID_HEAD_TOKENS
                        .iter()
                        .take(heads)
                        .map(|head| HeadCapability::new(*head))
                        .collect(),
                )
                .expect("inventory"),
            ),
            encoder,
        }
    }

    #[test]
    fn offer_is_produced_once_the_carrier_gate_is_open_and_the_operator_gate_is_on() {
        // MULTI_MONITOR_CARRIER_READY is `true` now that Carrier A is fully
        // wired end to end, so a fully opted-in operator gate (advertised
        // heads present) now genuinely produces an offer.
        assert!(build_offer(&enabled_gate(2)).is_some());
    }

    #[test]
    fn offer_is_withheld_when_the_operator_gate_is_disabled() {
        assert!(build_offer(&MultiMonitorGate::disabled()).is_none());
    }

    #[test]
    fn offer_is_withheld_when_no_heads_are_configured() {
        let gate = MultiMonitorGate {
            advertise_enabled: true,
            inventory: None,
            encoder: EncoderSelection::NativeNvenc,
        };
        assert!(build_offer(&gate).is_none());
    }

    #[test]
    fn offer_is_withheld_when_the_encoder_policy_is_auto() {
        // `Auto` can never be pinned to a concrete, exactly-supported
        // backend before session commit, so this host must never advertise
        // a capability it cannot honor.
        let gate = enabled_gate_with_encoder(2, EncoderSelection::Auto);
        assert!(build_offer(&gate).is_none());
    }

    #[test]
    fn offer_is_withheld_when_the_encoder_policy_is_windows_media_foundation() {
        // The Windows-only backend is never valid on this Linux host.
        let gate = enabled_gate_with_encoder(2, EncoderSelection::WindowsMediaFoundation);
        assert!(build_offer(&gate).is_none());
    }

    #[test]
    fn offer_is_produced_when_the_encoder_policy_is_pinned_software_h264() {
        // `SoftwareH264` is an offerable policy: whether a *specific*
        // requested geometry actually fits exactly is only knowable once a
        // concrete topology is planned, so the offer itself is not withheld
        // for this backend — only admission-time geometry is (see
        // `admit_request_rejects_software_h264_when_a_monitor_would_be_clamped`).
        let gate = enabled_gate_with_encoder(2, EncoderSelection::SoftwareH264);
        assert!(build_offer(&gate).is_some());
    }

    #[test]
    fn admission_degrades_to_not_requested_when_the_client_sends_nothing() {
        let gate = enabled_gate(2);
        assert_eq!(
            admit_requested_topology(&gate, None, None),
            MultiMonitorOutcome::NotRequested
        );
    }

    #[test]
    fn from_config_default_is_the_fully_disabled_gate() {
        let gate = MultiMonitorGate::from_config(
            &LinuxMultiMonitorConfig::default(),
            EncoderSelection::NativeNvenc,
        )
        .expect("default config is always valid");
        assert!(!gate.advertise_enabled);
        assert!(gate.inventory.is_none());
        assert!(build_offer(&gate).is_none());
    }

    #[test]
    fn from_config_with_enabled_flag_but_no_heads_yields_no_usable_inventory() {
        let config = LinuxMultiMonitorConfig {
            advertise_enabled: true,
            heads: Vec::new(),
            ..LinuxMultiMonitorConfig::default()
        };
        let gate = MultiMonitorGate::from_config(&config, EncoderSelection::NativeNvenc)
            .expect("valid config");
        assert!(gate.advertise_enabled);
        assert!(gate.usable_inventory().is_none());
    }

    #[test]
    fn from_config_builds_a_usable_inventory_from_valid_head_tokens() {
        let config = LinuxMultiMonitorConfig {
            advertise_enabled: true,
            heads: vec!["DFP-0".to_owned(), "DFP-1".to_owned()],
            ..LinuxMultiMonitorConfig::default()
        };
        let gate = MultiMonitorGate::from_config(&config, EncoderSelection::NativeNvenc)
            .expect("valid config");
        assert_eq!(
            gate.usable_inventory().expect("inventory").len(),
            2,
            "both configured heads must be present"
        );
    }

    #[test]
    fn from_config_rejects_an_invalid_head_token() {
        let config = LinuxMultiMonitorConfig {
            advertise_enabled: true,
            heads: vec!["HDMI-0".to_owned()],
            ..LinuxMultiMonitorConfig::default()
        };
        let error = MultiMonitorGate::from_config(&config, EncoderSelection::NativeNvenc)
            .expect_err("unknown head token must fail");
        assert_eq!(
            error,
            LinuxTopologyError::InvalidHeadToken("HDMI-0".to_owned())
        );
    }

    #[test]
    fn admission_plans_a_plannable_request_now_that_the_carrier_gate_is_open() {
        // MULTI_MONITOR_CARRIER_READY is `true` now that Carrier A is fully
        // wired end to end, so `admit_requested_topology` must now forward a
        // plannable request with a matching advertised offer straight through
        // the shared gate order to `Planned`, exactly as the post-gate steps
        // are already independently tested to do.
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let requested = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        let outcome = admit_requested_topology(&gate, Some(&offer), Some(&requested));
        match outcome {
            MultiMonitorOutcome::Planned { plan, carrier } => {
                assert_eq!(plan.monitors.len(), 1);
                assert_eq!(carrier, MultiMonitorCarrierMsg::MuxedReliableStream);
            }
            other => panic!("expected Planned, got {other:?}"),
        }
    }

    #[test]
    fn admission_degrades_when_this_host_never_advertised_an_offer_this_connection() {
        // The carrier gate being open does not bypass the rest of admission:
        // a request arriving without this host having ever advertised an
        // offer this connection still degrades, exactly as
        // `admit_request_degrades_when_this_host_never_advertised` already covers
        // directly for the post-gate steps.
        let gate = enabled_gate(2);
        let requested = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        assert_eq!(
            admit_requested_topology(&gate, None, Some(&requested)),
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::NotAdvertised)
        );
    }

    #[test]
    fn convert_requested_topology_round_trips_a_two_monitor_wire_roster() {
        let requested = RequestedMonitorTopologyMsg::new(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor(
                "b",
                2,
                (1920, 0),
                (1280, 720),
                false,
                RotationMsg::Degrees90,
            ),
        ])
        .expect("requested topology");
        let media = RequestedMonitorTopology::try_from(&requested).expect("converted topology");
        assert_eq!(media.monitors().len(), 2);
        assert_eq!(media.primary().monitor().width_px, 1920);
        assert_eq!(media.monitors()[1].monitor().rotation, Rotation::Degrees90);
    }

    fn sample_offer(max_monitors: u8) -> AuthMultiMonitorOfferMsg {
        AuthMultiMonitorOfferMsg::new(
            max_monitors,
            vec![
                RotationMsg::Degrees0,
                RotationMsg::Degrees90,
                RotationMsg::Degrees180,
                RotationMsg::Degrees270,
            ],
            OFFERED_CARRIERS.to_vec(),
        )
        .expect("offer")
    }

    fn sample_request(monitors: Vec<RequestedMonitorDescriptorMsg>) -> AuthMultiMonitorRequestMsg {
        AuthMultiMonitorRequestMsg::new(
            RequestedMonitorTopologyMsg::new(monitors).expect("requested topology"),
            OFFERED_CARRIERS.to_vec(),
        )
        .expect("auth request")
    }

    fn baseline_outcome_name(outcome: MultiMonitorOutcome) -> &'static str {
        match outcome {
            MultiMonitorOutcome::NotRequested => "not_requested",
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::GateDisabled) => {
                "gate_disabled"
            }
            MultiMonitorOutcome::Degraded(MultiMonitorDegradeReason::NoInventoryConfigured) => {
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
        let offer = build_offer(&enabled_gate(4)).expect("four-head offer");
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

        let one_monitor = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1_920, 1_080),
            true,
            RotationMsg::Degrees0,
        )]);
        let two_monitors = sample_request(vec![
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
        ]);
        let no_common_carrier = AuthMultiMonitorRequestMsg::new(
            one_monitor.requested_topology().clone(),
            vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
        )
        .unwrap();
        let no_inventory = MultiMonitorGate {
            advertise_enabled: true,
            inventory: None,
            encoder: EncoderSelection::NativeNvenc,
        };
        let outcomes = [
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(4),
                Some(&offer),
                None,
            )),
            baseline_outcome_name(admit_requested_topology(
                &MultiMonitorGate::disabled(),
                Some(&offer),
                Some(&one_monitor),
            )),
            baseline_outcome_name(admit_requested_topology(
                &no_inventory,
                Some(&offer),
                Some(&one_monitor),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(4),
                None,
                Some(&one_monitor),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(4),
                Some(&sample_offer(1)),
                Some(&two_monitors),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(4),
                Some(&offer),
                Some(&no_common_carrier),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(1),
                Some(&offer),
                Some(&two_monitors),
            )),
            baseline_outcome_name(admit_requested_topology(
                &enabled_gate(2),
                Some(&offer),
                Some(&two_monitors),
            )),
        ];
        assert_eq!(
            serde_json::to_value(outcomes).unwrap(),
            baseline["outcomes"]
        );
    }

    /// Drives the shared admission steps for a request this host definitely
    /// received, in the pre-share call shape, so the gate/offer/carrier/
    /// planning assertions below stay byte-for-byte the ones that guarded the
    /// host-local implementation before it moved into
    /// [`arcen_outputs::admission`].
    ///
    /// `requested` is always `Some` here, so the driver's `NotRequested` arm
    /// is unreachable; the end-to-end `NotRequested` path is covered by
    /// `admission_degrades_to_not_requested_when_the_client_sends_nothing`.
    fn admit_request(
        gate: &MultiMonitorGate,
        offer: Option<&AuthMultiMonitorOfferMsg>,
        requested: &AuthMultiMonitorRequestMsg,
    ) -> Result<(LinuxTopologyPlan, MultiMonitorCarrierMsg), MultiMonitorDegradeReason> {
        match admit_requested_topology(gate, offer, Some(requested)) {
            MultiMonitorOutcome::Planned { plan, carrier } => Ok((plan, carrier)),
            MultiMonitorOutcome::Degraded(reason) => Err(reason),
            MultiMonitorOutcome::NotRequested => {
                panic!("a request was supplied, so admission cannot be NotRequested")
            }
        }
    }

    // The following tests exercise the post-gate admission steps through
    // `admit_request` (rather than matching on the public
    // `admit_requested_topology`'s outcome enum) to isolate the
    // gate/offer/carrier-selection/planning logic from the
    // not-requested arm, which
    // `admission_plans_a_plannable_request_now_that_the_carrier_gate_is_open`
    // and `admission_degrades_when_this_host_never_advertised_an_offer_this_connection`
    // cover end to end through the public entry point instead.

    #[test]
    fn admit_request_degrades_when_the_operator_gate_is_disabled() {
        let gate = MultiMonitorGate::disabled();
        let offer = sample_offer(2);
        let request = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        assert_eq!(
            admit_request(&gate, Some(&offer), &request),
            Err(MultiMonitorDegradeReason::GateDisabled)
        );
    }

    #[test]
    fn admit_request_degrades_when_no_inventory_is_configured() {
        let gate = MultiMonitorGate {
            advertise_enabled: true,
            inventory: None,
            encoder: EncoderSelection::NativeNvenc,
        };
        let offer = sample_offer(2);
        let request = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        assert_eq!(
            admit_request(&gate, Some(&offer), &request),
            Err(MultiMonitorDegradeReason::NoInventoryConfigured)
        );
    }

    #[test]
    fn admit_request_degrades_when_this_host_never_advertised() {
        let gate = enabled_gate(2);
        let request = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        assert_eq!(
            admit_request(&gate, None, &request),
            Err(MultiMonitorDegradeReason::NotAdvertised)
        );
    }

    #[test]
    fn admit_request_degrades_when_the_request_exceeds_the_advertised_offer() {
        let gate = enabled_gate(2);
        // Only 1 monitor advertised, but the request asks for 2.
        let offer = sample_offer(1);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let error = admit_request(&gate, Some(&offer), &request).expect_err("must fail");
        assert!(matches!(
            error,
            MultiMonitorDegradeReason::ExceedsAdvertisedOffer(_)
        ));
    }

    #[test]
    fn admit_request_degrades_when_no_common_carrier_exists() {
        let gate = enabled_gate(2);
        let offer = AuthMultiMonitorOfferMsg::new(
            2,
            vec![RotationMsg::Degrees0],
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("offer");
        let request = AuthMultiMonitorRequestMsg::new(
            RequestedMonitorTopologyMsg::new(vec![descriptor(
                "primary",
                1,
                (0, 0),
                (1920, 1080),
                true,
                RotationMsg::Degrees0,
            )])
            .expect("requested topology"),
            vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
        )
        .expect("auth request");
        assert_eq!(
            admit_request(&gate, Some(&offer), &request),
            Err(MultiMonitorDegradeReason::ExceedsAdvertisedOffer(
                MultiMonitorValidationError::NoCommonAuthCarrier
            ))
        );
    }

    #[test]
    fn admit_request_selects_the_common_carrier_and_plans_the_topology() {
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let (plan, carrier) =
            admit_request(&gate, Some(&offer), &request).expect("request must be admitted");
        assert_eq!(carrier, MultiMonitorCarrierMsg::MuxedReliableStream);
        assert_eq!(plan.monitors.len(), 2);
        assert_eq!(plan.monitors[0].session_monitor_id.get(), 1);
        assert_eq!(plan.monitors[1].session_monitor_id.get(), 2);
    }

    #[test]
    fn admit_request_fails_the_whole_request_atomically_when_any_monitor_cannot_be_planned() {
        // The advertised offer permits up to 4 monitors (so the request
        // clears offer/carrier validation), but this host's configured head
        // inventory only has 2 heads: the 3-monitor request must fail
        // wholesale during planning — no partial 2-of-3 plan is ever
        // produced.
        let gate = enabled_gate(2);
        let offer = sample_offer(4);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
            descriptor("c", 3, (3200, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let error = admit_request(&gate, Some(&offer), &request).expect_err("must fail");
        assert!(matches!(
            error,
            MultiMonitorDegradeReason::PlanningFailed(LinuxTopologyError::InsufficientHeads { .. })
        ));
    }

    #[test]
    fn admit_request_rejects_software_h264_when_a_monitor_would_be_clamped() {
        // 2560x1600 exceeds OpenH264's 1920x1080 contract (see
        // `media::multi_capenc`'s own clamp-detection fixtures): this
        // experimental v1 must fail admission closed rather than silently
        // commit a topology one of its pipelines cannot exactly encode.
        let gate = enabled_gate_with_encoder(2, EncoderSelection::SoftwareH264);
        let offer = sample_offer(2);
        let request = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (2560, 1600),
            true,
            RotationMsg::Degrees0,
        )]);
        let error = admit_request(&gate, Some(&offer), &request)
            .expect_err("clamping geometry must be rejected");
        assert!(matches!(
            error,
            MultiMonitorDegradeReason::EncoderPolicyRejected(
                crate::media::multi_capenc::MultiCapencConfigError::SoftwareGeometryWouldClamp {
                    width: 2560,
                    height: 1600,
                    ..
                }
            )
        ));
    }

    #[test]
    fn admit_request_accepts_software_h264_when_geometry_fits_exactly() {
        // 1920x1080 fits OpenH264's contract with no clamp, so this policy
        // may exactly honor it: admission must succeed.
        let gate = enabled_gate_with_encoder(2, EncoderSelection::SoftwareH264);
        let offer = sample_offer(2);
        let request = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        let (plan, _carrier) = admit_request(&gate, Some(&offer), &request)
            .expect("exact-fit software geometry must be admitted");
        assert_eq!(plan.monitors.len(), 1);
    }

    #[test]
    fn admit_request_accepts_native_nvenc_for_any_geometry() {
        // `NativeNvenc` is pinned with no software fallback, so it is never
        // subject to the OpenH264 clamp preflight: even geometry that would
        // clamp under `SoftwareH264` must be admitted unchanged.
        let gate = enabled_gate_with_encoder(2, EncoderSelection::NativeNvenc);
        let offer = sample_offer(2);
        let request = sample_request(vec![descriptor(
            "primary",
            1,
            (0, 0),
            (2560, 1600),
            true,
            RotationMsg::Degrees0,
        )]);
        let (plan, _carrier) = admit_request(&gate, Some(&offer), &request)
            .expect("nvenc-pinned geometry must be admitted regardless of size");
        assert_eq!(plan.monitors.len(), 1);
    }

    fn resolved_plan(width: u32, height: u32, fps: u32) -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend: arcen_media::video::EncoderBackend::NativeNvenc,
            video: arcen_media::VideoConfiguration::legacy_h264(),
            width,
            height,
            fps,
            codecs: arcen_media::CodecSet::from_slice(&[arcen_media::VideoCodec::H264]),
            chroma: arcen_media::ChromaSet::from_slice(&[arcen_media::ChromaSubsampling::Yuv420]),
            bit_depths: arcen_media::BitDepthSet::from_slice(&[arcen_media::BitDepth::Eight]),
            ranges: arcen_media::ColorRangeSet::from_slice(&[arcen_media::ColorRange::Limited]),
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            cursor_in_video: false,
        }
    }

    /// The negotiated roster the session's encoder set was admitted with,
    /// built the same way `media::encoder_admission` builds it for this exact
    /// plan (one region per planned monitor, at the planned geometry).
    fn negotiated_roster(plan: &LinuxTopologyPlan) -> RegionMediaRoster {
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
                        arcen_media::BitrateBudgetKbps::nominal_for_geometry(
                            monitor.width,
                            monitor.height,
                            60,
                        ),
                    )
                    .expect("valid region media plan")
                })
                .collect(),
        )
        .expect("valid negotiated roster")
    }

    #[test]
    fn applied_media_plan_bitrate_is_the_negotiated_region_budget_verbatim() {
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let (plan, carrier) =
            admit_request(&gate, Some(&offer), &request).expect("request must be admitted");
        let media: Vec<(SessionMonitorId, ResolvedMediaPlan)> = plan
            .monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    resolved_plan(monitor.width, monitor.height, 60),
                )
            })
            .collect();
        let negotiated = negotiated_roster(&plan);

        let capability = build_applied_capability(&gate, &plan, carrier, &media, &negotiated)
            .expect("applied capability must build");
        let applied = capability
            .applied_topology()
            .expect("applied topology must be present");
        for monitor in applied.monitors() {
            let negotiated_plan = negotiated
                .plan(
                    SessionMonitorId::new(monitor.session_monitor_id)
                        .expect("published monitor id must be nonzero"),
                )
                .expect("every applied monitor has a negotiated plan");
            assert_eq!(
                monitor.media_plan.bitrate_kbps,
                negotiated_plan.applied_bitrate_kbps(),
                "the published bitrate is the negotiated budget, read verbatim"
            );
            assert_eq!(
                arcen_media::BitrateBudgetKbps::new(monitor.media_plan.bitrate_kbps),
                Ok(negotiated_plan.bitrate_budget),
                "every published bitrate must round-trip through the value object"
            );
        }
    }

    #[test]
    fn applied_media_plan_bitrate_follows_the_negotiated_budget_not_the_applied_geometry() {
        // The proof that nothing recomputes a nominal budget at assembly
        // time: hand assembly a roster whose budgets are deliberately not the
        // nominal ones for the applied geometry and require the wire to carry
        // the negotiated numbers anyway.
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let (plan, carrier) =
            admit_request(&gate, Some(&offer), &request).expect("request must be admitted");
        let media: Vec<(SessionMonitorId, ResolvedMediaPlan)> = plan
            .monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    resolved_plan(monitor.width, monitor.height, 60),
                )
            })
            .collect();
        let nominal = negotiated_roster(&plan);
        let negotiated = RegionMediaRoster::new(
            nominal
                .plans()
                .iter()
                .map(|region| {
                    let mut region = *region;
                    region.bitrate_budget = arcen_media::BitrateBudgetKbps::new(
                        arcen_media::BitrateBudgetKbps::NOMINAL_FLOOR_KBPS,
                    )
                    .expect("floor is an in-band budget");
                    region
                })
                .collect(),
        )
        .expect("valid negotiated roster");
        for region in nominal.plans() {
            assert_ne!(
                region.applied_bitrate_kbps(),
                arcen_media::BitrateBudgetKbps::NOMINAL_FLOOR_KBPS,
                "the test's negotiated budget must actually differ from the nominal one"
            );
        }

        let capability = build_applied_capability(&gate, &plan, carrier, &media, &negotiated)
            .expect("applied capability must build");
        let applied = capability
            .applied_topology()
            .expect("applied topology must be present");
        assert!(
            applied
                .monitors()
                .iter()
                .all(|monitor| monitor.media_plan.bitrate_kbps
                    == arcen_media::BitrateBudgetKbps::NOMINAL_FLOOR_KBPS),
            "the applied capability must publish the negotiated budget, never a re-derived one"
        );
    }

    #[test]
    fn build_applied_capability_assembles_a_valid_server_multi_monitor_msg_for_a_planned_topology()
    {
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let (plan, carrier) =
            admit_request(&gate, Some(&offer), &request).expect("request must be admitted");
        let media: Vec<(SessionMonitorId, ResolvedMediaPlan)> = plan
            .monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    resolved_plan(monitor.width, monitor.height, 60),
                )
            })
            .collect();
        let negotiated = negotiated_roster(&plan);

        let capability = build_applied_capability(&gate, &plan, carrier, &media, &negotiated)
            .expect("applied capability must build");
        assert_eq!(capability.max_monitors(), 2);
        assert_eq!(
            capability
                .applied_topology()
                .expect("applied topology must be present")
                .monitors()
                .len(),
            2
        );
        assert!(capability
            .applied_topology()
            .expect("applied topology")
            .monitors()
            .iter()
            .all(|monitor| monitor.media_plan.stream_epoch == plan.generation.get()));
    }

    #[test]
    fn build_applied_capability_fails_when_a_planned_monitor_has_no_matching_media_plan() {
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let request = sample_request(vec![
            descriptor("a", 1, (0, 0), (1920, 1080), true, RotationMsg::Degrees0),
            descriptor("b", 2, (1920, 0), (1280, 720), false, RotationMsg::Degrees0),
        ]);
        let (plan, carrier) =
            admit_request(&gate, Some(&offer), &request).expect("request must be admitted");
        // Only one of the two planned monitors has a resolved media plan.
        let media = vec![(
            plan.monitors[0].session_monitor_id,
            resolved_plan(1920, 1080, 60),
        )];
        let negotiated = negotiated_roster(&plan);

        let error = build_applied_capability(&gate, &plan, carrier, &media, &negotiated)
            .expect_err("must fail when a monitor's media plan is missing");
        assert!(matches!(error, AppliedCapabilityError::MissingMediaPlan(_)));
    }

    #[test]
    fn build_applied_capability_fails_when_the_gate_has_no_usable_inventory() {
        let gate = enabled_gate(2);
        let offer = sample_offer(2);
        let request = sample_request(vec![descriptor(
            "a",
            1,
            (0, 0),
            (1920, 1080),
            true,
            RotationMsg::Degrees0,
        )]);
        let (plan, carrier) =
            admit_request(&gate, Some(&offer), &request).expect("request must be admitted");
        let media = vec![(
            plan.monitors[0].session_monitor_id,
            resolved_plan(1920, 1080, 60),
        )];
        let negotiated = negotiated_roster(&plan);

        let empty_gate = MultiMonitorGate {
            advertise_enabled: true,
            inventory: None,
            encoder: EncoderSelection::NativeNvenc,
        };
        let error = build_applied_capability(&empty_gate, &plan, carrier, &media, &negotiated)
            .expect_err("must fail without a usable inventory");
        assert_eq!(error, AppliedCapabilityError::NoInventoryConfigured);
    }
}
