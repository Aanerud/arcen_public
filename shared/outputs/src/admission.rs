//! Admission orchestration policy.
//!
//! Both hosts hand-wrote the same decision shape for the multi-region
//! capability: an unconditional carrier gate, then an operator opt-in, then an
//! inventory check, then "did this host actually advertise it", then carrier
//! intersection, then planning — and every refusal degraded atomically to the
//! host's existing single-primary behaviour rather than applying a partial
//! topology.
//!
//! This module owns the gate ordering, the outcome shape, the degrade
//! attribution, and the carrier intersection rule, so two hosts cannot drift
//! into refusing the same request in a different order or for differently
//! worded reasons. It owns nothing else, and in particular it depends on no
//! host inventory, no protocol message, and no network runtime:
//!
//! - The gates are plain booleans a host computes from its own configuration
//!   and its own freshly probed inventory.
//! - Everything that can only be decided by reading a host plan, a protocol
//!   offer, or an encoder policy is a caller-supplied closure that returns a
//!   host-typed [`AdmissionRejection`], attributed to an [`AdmissionStage`].
//! - The carrier is a caller-chosen type. This module only implements the
//!   intersection rule: the first carrier this host prefers that the client
//!   also supports.
//!
//! Admission never produces a partial result. [`admit`] either returns a
//! fully planned [`AdmissionOutcome::Admitted`] or degrades, which is the
//! ADR 0009 atomic-topology invariant at the decision boundary.
//!
//! [`admit`] is the bare two-callback form. [`admit_regions`] is the
//! multi-region form both hosts actually run: it adds the frozen steps that
//! sit between carrier selection and a committed plan — converting the wire
//! request into the shared [`arcen_media::RequestedMonitorTopology`], stamping
//! the session's first [`arcen_media::TopologyGeneration`], planning, and a
//! trailing media-policy check for hosts that have one — behind the
//! [`RegionAdmissionPolicy`] trait.

use core::fmt;

use arcen_media::{RequestedMonitorTopology, TopologyGeneration};

/// The host-independent gates that must all be open before a request can be
/// admitted.
///
/// Every field is a fact the host already knows before it looks at the
/// request.
// Each gate is an independent fact with its own typed `GateClosed` reason, and
// `evaluate` reads them in a frozen order, so a state machine would hide the
// ordering this type exists to freeze.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdmissionGates {
    /// The carrier that delivers multi-region media is implemented and
    /// enabled on this host.
    pub carrier_ready: bool,
    /// The operator explicitly opted this host in. Defaults to closed.
    pub operator_enabled: bool,
    /// This host has a usable, non-empty output inventory to plan against.
    pub inventory_available: bool,
    /// This host actually advertised the capability on this connection.
    pub offer_advertised: bool,
}

impl AdmissionGates {
    /// Every gate closed: the default, safe posture.
    pub const CLOSED: Self = Self {
        carrier_ready: false,
        operator_enabled: false,
        inventory_available: false,
        offer_advertised: false,
    };

    /// Every gate open.
    pub const OPEN: Self = Self {
        carrier_ready: true,
        operator_enabled: true,
        inventory_available: true,
        offer_advertised: true,
    };

    /// Whether this host may advertise the capability before authentication.
    ///
    /// The pre-authentication offer does not depend on
    /// [`Self::offer_advertised`], which is the record of that decision.
    /// Withholding the offer, rather than only refusing admission later,
    /// means a host never advertises a capability it cannot deliver, so a
    /// client never even attempts to request it.
    #[must_use]
    pub const fn may_advertise(&self) -> bool {
        self.carrier_ready && self.operator_enabled && self.inventory_available
    }

    /// Checks the gates in their frozen order.
    ///
    /// The carrier gate is checked first and unconditionally, so a host whose
    /// carrier is not enabled behaves exactly like a host with the capability
    /// switched off, regardless of configuration or request.
    ///
    /// # Errors
    ///
    /// Returns the first [`GateClosed`] in that order.
    pub const fn evaluate(&self) -> Result<(), GateClosed> {
        if !self.carrier_ready {
            return Err(GateClosed::CarrierNotReady);
        }
        if !self.operator_enabled {
            return Err(GateClosed::OperatorDisabled);
        }
        if !self.inventory_available {
            return Err(GateClosed::NoInventoryAvailable);
        }
        if !self.offer_advertised {
            return Err(GateClosed::NotAdvertised);
        }
        Ok(())
    }
}

/// Which host-independent gate refused the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GateClosed {
    /// The multi-region carrier is not enabled on this host.
    CarrierNotReady,
    /// The operator did not opt this host in.
    OperatorDisabled,
    /// This host has no usable output inventory to plan against.
    NoInventoryAvailable,
    /// The client requested a capability this host never advertised.
    NotAdvertised,
}

impl fmt::Display for GateClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CarrierNotReady => "the multi-region carrier is not enabled on this host",
            Self::OperatorDisabled => "the multi-region gate is disabled on this host",
            Self::NoInventoryAvailable => "no output inventory is available on this host",
            Self::NotAdvertised => "the client requested a capability this host never advertised",
        })
    }
}

impl std::error::Error for GateClosed {}

/// Which host-typed step refused the request.
///
/// `#[non_exhaustive]` so new steps are additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdmissionStage {
    /// Validating the request against the offer this host advertised.
    Offer,
    /// Selecting a carrier both sides support.
    Carrier,
    /// Converting the request into this host's validated request type.
    Request,
    /// Planning the request onto this host's inventory.
    Planning,
    /// Checking the planned geometry against this host's media policy.
    MediaPolicy,
}

impl fmt::Display for AdmissionStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Offer => "offer",
            Self::Carrier => "carrier",
            Self::Request => "request",
            Self::Planning => "planning",
            Self::MediaPolicy => "media policy",
        })
    }
}

/// A host-typed refusal, attributed to the step that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionRejection<E> {
    /// Which step refused.
    pub stage: AdmissionStage,
    /// The host's own typed reason.
    pub source: E,
}

impl<E> AdmissionRejection<E> {
    /// Attributes a host refusal to a step.
    #[must_use]
    pub const fn new(stage: AdmissionStage, source: E) -> Self {
        Self { stage, source }
    }
}

impl<E: fmt::Display> fmt::Display for AdmissionRejection<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} refused the request: {}",
            self.stage, self.source
        )
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for AdmissionRejection<E> {}

/// Why a request degraded to this host's existing single-region behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason<E> {
    /// A host-independent gate was closed.
    Gate(GateClosed),
    /// A host-typed step refused.
    Rejected(AdmissionRejection<E>),
}

impl<E> DegradeReason<E> {
    /// The closed gate, when a gate was the reason.
    #[must_use]
    pub const fn gate(&self) -> Option<GateClosed> {
        match self {
            Self::Gate(gate) => Some(*gate),
            Self::Rejected(_) => None,
        }
    }

    /// The refusing step, when a host step was the reason.
    #[must_use]
    pub const fn stage(&self) -> Option<AdmissionStage> {
        match self {
            Self::Gate(_) => None,
            Self::Rejected(rejection) => Some(rejection.stage),
        }
    }

    /// The host's own typed reason, when a host step was the reason.
    #[must_use]
    pub const fn source(&self) -> Option<&E> {
        match self {
            Self::Gate(_) => None,
            Self::Rejected(rejection) => Some(&rejection.source),
        }
    }
}

impl<E> From<GateClosed> for DegradeReason<E> {
    fn from(gate: GateClosed) -> Self {
        Self::Gate(gate)
    }
}

impl<E> From<AdmissionRejection<E>> for DegradeReason<E> {
    fn from(rejection: AdmissionRejection<E>) -> Self {
        Self::Rejected(rejection)
    }
}

impl<E: fmt::Display> fmt::Display for DegradeReason<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gate(gate) => write!(formatter, "{gate}"),
            Self::Rejected(rejection) => write!(formatter, "{rejection}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for DegradeReason<E> {}

/// What a host does with a client's optional multi-region request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionOutcome<P, C, E> {
    /// The client did not request the capability. This host's legacy default
    /// behaviour applies completely unchanged.
    NotRequested,
    /// The client requested it, and this host falls back to its existing
    /// single-region behaviour for a documented, typed reason. No partial
    /// topology is ever applied.
    Degraded(DegradeReason<E>),
    /// The request was fully, atomically planned, using the host-selected
    /// element of the client and host common carrier set.
    Admitted { plan: P, carrier: C },
}

impl<P, C, E> AdmissionOutcome<P, C, E> {
    /// Whether the request was admitted.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }

    /// The admitted plan and carrier, or `None` for every other outcome.
    #[must_use]
    pub fn into_admitted(self) -> Option<(P, C)> {
        match self {
            Self::Admitted { plan, carrier } => Some((plan, carrier)),
            Self::NotRequested | Self::Degraded(_) => None,
        }
    }

    /// Why the request degraded, or `None` when it did not.
    #[must_use]
    pub const fn degrade_reason(&self) -> Option<&DegradeReason<E>> {
        match self {
            Self::Degraded(reason) => Some(reason),
            Self::NotRequested | Self::Admitted { .. } => None,
        }
    }
}

/// Why no carrier could be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CarrierMismatch {
    /// This host offered no carrier at all.
    HostOffersNone,
    /// The client declared support for no carrier at all.
    ClientSupportsNone,
    /// The two sets do not intersect.
    NoCommonCarrier,
}

impl fmt::Display for CarrierMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HostOffersNone => "this host offers no multi-region carrier",
            Self::ClientSupportsNone => "the client supports no multi-region carrier",
            Self::NoCommonCarrier => "the client and this host share no multi-region carrier",
        })
    }
}

impl std::error::Error for CarrierMismatch {}

/// Selects the first carrier this host prefers that the client also supports.
///
/// Host preference decides, not client order: the host is the side that has to
/// deliver the media. The rule is deterministic, so the same pair of sets
/// always selects the same carrier.
///
/// # Errors
///
/// Returns [`CarrierMismatch`] when either side offers nothing, or when the
/// two sets do not intersect.
pub fn select_carrier<C: Copy + Eq>(
    host_preference: &[C],
    client_supported: &[C],
) -> Result<C, CarrierMismatch> {
    if host_preference.is_empty() {
        return Err(CarrierMismatch::HostOffersNone);
    }
    if client_supported.is_empty() {
        return Err(CarrierMismatch::ClientSupportsNone);
    }
    host_preference
        .iter()
        .find(|carrier| client_supported.contains(carrier))
        .copied()
        .ok_or(CarrierMismatch::NoCommonCarrier)
}

/// Runs the frozen admission order over one optional request.
///
/// The order is: no request at all, then the host-independent gates in
/// [`AdmissionGates::evaluate`] order, then `select`, then `plan`. Neither
/// closure runs until every gate is open, and `plan` does not run unless
/// `select` succeeded, so a host can never plan against a carrier it has not
/// agreed on.
///
/// `select` and `plan` are where everything host-shaped lives: validating the
/// request against the advertised offer, choosing a carrier through
/// [`select_carrier`], converting the request into the host's own validated
/// type, planning it onto the host's inventory, and checking the planned
/// geometry against the host's media policy. Each returns an
/// [`AdmissionRejection`] naming the step that refused.
#[must_use]
pub fn admit<R, P, C, E>(
    gates: AdmissionGates,
    requested: Option<R>,
    select: impl FnOnce(&R) -> Result<C, AdmissionRejection<E>>,
    plan: impl FnOnce(R, &C) -> Result<P, AdmissionRejection<E>>,
) -> AdmissionOutcome<P, C, E> {
    let Some(requested) = requested else {
        return AdmissionOutcome::NotRequested;
    };
    if let Err(gate) = gates.evaluate() {
        return AdmissionOutcome::Degraded(DegradeReason::Gate(gate));
    }
    let carrier = match select(&requested) {
        Ok(carrier) => carrier,
        Err(rejection) => return AdmissionOutcome::Degraded(DegradeReason::Rejected(rejection)),
    };
    match plan(requested, &carrier) {
        Ok(plan) => AdmissionOutcome::Admitted { plan, carrier },
        Err(rejection) => AdmissionOutcome::Degraded(DegradeReason::Rejected(rejection)),
    }
}

/// The host-shaped half of the frozen multi-region admission order.
///
/// [`admit_regions`] drives these steps; a host implements them and never
/// re-states the order, the stage attribution, or the atomic-degrade rule.
/// Every step returns the host's own typed refusal, which the driver attaches
/// to the [`AdmissionStage`] that produced it.
///
/// The request itself stays host-typed ([`Self::Request`]) because it arrives
/// on a wire this crate does not model. The only thing the driver requires of
/// it is that the host can convert it into the shared, validated
/// [`RequestedMonitorTopology`], which is where the host-independent topology
/// contract begins.
pub trait RegionAdmissionPolicy {
    /// The wire-shaped request sidecar this host received.
    type Request: ?Sized;
    /// The carrier this host applies for an admitted request.
    type Carrier: Copy + Eq;
    /// This host's own validated topology plan.
    type Plan;
    /// This host's own typed refusal.
    type Rejection;

    /// Validates the request against the exact offer this host advertised on
    /// this connection ([`AdmissionStage::Offer`]).
    ///
    /// # Errors
    ///
    /// Returns this host's typed refusal when the request does not fit the
    /// offer, or when the recorded offer is itself unusable.
    fn validate_advertised_offer(&self, requested: &Self::Request) -> Result<(), Self::Rejection>;

    /// Selects the carrier both sides support ([`AdmissionStage::Carrier`]).
    ///
    /// # Errors
    ///
    /// Returns this host's typed refusal when the two carrier sets do not
    /// intersect.
    fn select_carrier(&self, requested: &Self::Request) -> Result<Self::Carrier, Self::Rejection>;

    /// Converts the wire request into the shared validated topology
    /// ([`AdmissionStage::Request`]).
    ///
    /// # Errors
    ///
    /// Returns this host's typed refusal when the requested roster violates
    /// the shared topology contract.
    fn convert_request(
        &self,
        requested: &Self::Request,
    ) -> Result<RequestedMonitorTopology, Self::Rejection>;

    /// Plans the converted topology onto this host's own inventory
    /// ([`AdmissionStage::Planning`]).
    ///
    /// `requested` is passed back so a host can carry request-only fields
    /// (quality intent, for instance) onto its plan without re-deriving them.
    ///
    /// # Errors
    ///
    /// Returns this host's typed refusal when the topology cannot be planned
    /// in full. Planning is atomic: a host never returns a partial plan.
    fn plan_topology(
        &self,
        requested: &Self::Request,
        topology: &RequestedMonitorTopology,
        generation: TopologyGeneration,
    ) -> Result<Self::Plan, Self::Rejection>;

    /// Checks the planned geometry against this host's media policy
    /// ([`AdmissionStage::MediaPolicy`]).
    ///
    /// Runs last, after planning, because a media policy can only judge the
    /// exact geometry a plan committed to. Defaults to accepting: a host
    /// without an encoder-policy gate implements nothing.
    ///
    /// # Errors
    ///
    /// Returns this host's typed refusal when the committed geometry is not
    /// exactly supported by its configured media policy.
    fn check_media_policy(&self, _plan: &Self::Plan) -> Result<(), Self::Rejection> {
        Ok(())
    }
}

/// Runs the frozen multi-region admission order over one optional request.
///
/// The order is: no request at all, then the host-independent gates in
/// [`AdmissionGates::evaluate`] order, then the [`RegionAdmissionPolicy`]
/// steps in [`AdmissionStage`] order — offer, carrier, request conversion,
/// planning, media policy. No step runs until every gate is open, and no step
/// runs after an earlier one refused, so a host can never plan against a
/// carrier it has not agreed on, nor apply a topology its media policy
/// rejects.
///
/// Every session's first committed topology is
/// [`TopologyGeneration::FIRST`]; multi-region topology is fixed for the
/// session's lifetime, with no live re-negotiation.
///
/// The result is never partial: the request is either fully admitted or
/// degrades to the host's existing single-region behaviour, which is the
/// ADR 0009 atomic-topology invariant at the decision boundary.
#[must_use]
pub fn admit_regions<P: RegionAdmissionPolicy + ?Sized>(
    gates: AdmissionGates,
    policy: &P,
    requested: Option<&P::Request>,
) -> AdmissionOutcome<P::Plan, P::Carrier, P::Rejection> {
    let Some(requested) = requested else {
        return AdmissionOutcome::NotRequested;
    };
    if let Err(gate) = gates.evaluate() {
        return AdmissionOutcome::Degraded(DegradeReason::Gate(gate));
    }
    if let Err(source) = policy.validate_advertised_offer(requested) {
        return degraded(AdmissionStage::Offer, source);
    }
    let carrier = match policy.select_carrier(requested) {
        Ok(carrier) => carrier,
        Err(source) => return degraded(AdmissionStage::Carrier, source),
    };
    let topology = match policy.convert_request(requested) {
        Ok(topology) => topology,
        Err(source) => return degraded(AdmissionStage::Request, source),
    };
    let plan = match policy.plan_topology(requested, &topology, TopologyGeneration::FIRST) {
        Ok(plan) => plan,
        Err(source) => return degraded(AdmissionStage::Planning, source),
    };
    if let Err(source) = policy.check_media_policy(&plan) {
        return degraded(AdmissionStage::MediaPolicy, source);
    }
    AdmissionOutcome::Admitted { plan, carrier }
}

fn degraded<P, C, E>(stage: AdmissionStage, source: E) -> AdmissionOutcome<P, C, E> {
    AdmissionOutcome::Degraded(DegradeReason::Rejected(AdmissionRejection::new(
        stage, source,
    )))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{
        AdmissionGates, AdmissionOutcome, AdmissionRejection, AdmissionStage, CarrierMismatch,
        DegradeReason, GateClosed, admit, select_carrier,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Carrier {
        Muxed,
        PerRegion,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Request {
        regions: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Plan {
        regions: usize,
        carrier: Carrier,
    }

    #[derive(Debug, Default)]
    struct Steps {
        ran: RefCell<Vec<&'static str>>,
    }

    impl Steps {
        fn note(&self, step: &'static str) {
            self.ran.borrow_mut().push(step);
        }

        fn ran(&self) -> Vec<&'static str> {
            self.ran.borrow().clone()
        }
    }

    fn run(
        gates: AdmissionGates,
        requested: Option<Request>,
        reject: Option<AdmissionRejection<&'static str>>,
    ) -> (Steps, AdmissionOutcome<Plan, Carrier, &'static str>) {
        let steps = Steps::default();
        let outcome = admit(
            gates,
            requested,
            |_request| {
                steps.note("select");
                match reject {
                    Some(rejection) if rejection.stage == AdmissionStage::Carrier => Err(rejection),
                    _ => Ok(Carrier::Muxed),
                }
            },
            |request, carrier| {
                steps.note("plan");
                match reject {
                    Some(rejection) if rejection.stage != AdmissionStage::Carrier => Err(rejection),
                    _ => Ok(Plan {
                        regions: request.regions,
                        carrier: *carrier,
                    }),
                }
            },
        );
        (steps, outcome)
    }

    #[test]
    fn no_request_leaves_legacy_behaviour_completely_unchanged() {
        let (steps, outcome) = run(AdmissionGates::OPEN, None, None);
        assert_eq!(outcome, AdmissionOutcome::NotRequested);
        assert!(!outcome.is_admitted());
        assert!(outcome.degrade_reason().is_none());
        assert!(steps.ran().is_empty());
    }

    #[test]
    fn every_gate_is_checked_in_its_frozen_order_before_any_host_step() {
        let cases = [
            (AdmissionGates::CLOSED, GateClosed::CarrierNotReady),
            (
                AdmissionGates {
                    carrier_ready: true,
                    ..AdmissionGates::CLOSED
                },
                GateClosed::OperatorDisabled,
            ),
            (
                AdmissionGates {
                    carrier_ready: true,
                    operator_enabled: true,
                    ..AdmissionGates::CLOSED
                },
                GateClosed::NoInventoryAvailable,
            ),
            (
                AdmissionGates {
                    offer_advertised: false,
                    ..AdmissionGates::OPEN
                },
                GateClosed::NotAdvertised,
            ),
        ];
        for (gates, expected) in cases {
            let (steps, outcome) = run(gates, Some(Request { regions: 2 }), None);
            assert_eq!(
                outcome,
                AdmissionOutcome::Degraded(DegradeReason::Gate(expected))
            );
            assert_eq!(
                outcome.degrade_reason().and_then(DegradeReason::gate),
                Some(expected)
            );
            assert!(
                steps.ran().is_empty(),
                "no host step runs while a gate is closed"
            );
        }
    }

    #[test]
    fn the_carrier_gate_outranks_every_operator_choice() {
        let gates = AdmissionGates {
            carrier_ready: false,
            ..AdmissionGates::OPEN
        };
        let (_steps, outcome) = run(gates, Some(Request { regions: 4 }), None);
        assert_eq!(
            outcome.degrade_reason().and_then(DegradeReason::gate),
            Some(GateClosed::CarrierNotReady)
        );
    }

    #[test]
    fn an_open_gate_set_plans_the_request_atomically() {
        let (steps, outcome) = run(AdmissionGates::OPEN, Some(Request { regions: 3 }), None);
        assert!(outcome.is_admitted());
        assert_eq!(steps.ran(), ["select", "plan"]);
        assert_eq!(
            outcome.into_admitted(),
            Some((
                Plan {
                    regions: 3,
                    carrier: Carrier::Muxed,
                },
                Carrier::Muxed
            ))
        );
    }

    #[test]
    fn a_carrier_rejection_never_reaches_planning() {
        let rejection = AdmissionRejection::new(AdmissionStage::Carrier, "no common carrier");
        let (steps, outcome) = run(
            AdmissionGates::OPEN,
            Some(Request { regions: 2 }),
            Some(rejection),
        );
        assert_eq!(
            outcome,
            AdmissionOutcome::Degraded(DegradeReason::Rejected(rejection))
        );
        assert_eq!(steps.ran(), ["select"]);
        let reason = outcome.degrade_reason().expect("degraded");
        assert_eq!(reason.stage(), Some(AdmissionStage::Carrier));
        assert_eq!(reason.source(), Some(&"no common carrier"));
        assert_eq!(reason.gate(), None);
    }

    #[test]
    fn every_host_step_keeps_its_own_attribution() {
        for stage in [
            AdmissionStage::Offer,
            AdmissionStage::Request,
            AdmissionStage::Planning,
            AdmissionStage::MediaPolicy,
        ] {
            let rejection = AdmissionRejection::new(stage, "host refused");
            let (steps, outcome) = run(
                AdmissionGates::OPEN,
                Some(Request { regions: 2 }),
                Some(rejection),
            );
            assert_eq!(steps.ran(), ["select", "plan"]);
            let reason = outcome.degrade_reason().expect("degraded");
            assert_eq!(reason.stage(), Some(stage));
            assert_eq!(reason.source(), Some(&"host refused"));
            assert!(reason.to_string().contains("host refused"));
            assert!(outcome.into_admitted().is_none());
        }
    }

    #[test]
    fn advertising_needs_the_carrier_operator_and_inventory_gates_but_not_the_offer_record() {
        assert!(!AdmissionGates::CLOSED.may_advertise());
        assert!(AdmissionGates::OPEN.may_advertise());
        assert!(
            AdmissionGates {
                offer_advertised: false,
                ..AdmissionGates::OPEN
            }
            .may_advertise(),
            "the offer record is the result of advertising, not a precondition"
        );
        for closed in [
            AdmissionGates {
                carrier_ready: false,
                ..AdmissionGates::OPEN
            },
            AdmissionGates {
                operator_enabled: false,
                ..AdmissionGates::OPEN
            },
            AdmissionGates {
                inventory_available: false,
                ..AdmissionGates::OPEN
            },
        ] {
            assert!(!closed.may_advertise());
        }
        assert_eq!(AdmissionGates::default(), AdmissionGates::CLOSED);
    }

    #[test]
    fn carrier_selection_follows_host_preference_deterministically() {
        assert_eq!(
            select_carrier(
                &[Carrier::Muxed, Carrier::PerRegion],
                &[Carrier::PerRegion, Carrier::Muxed]
            ),
            Ok(Carrier::Muxed)
        );
        assert_eq!(
            select_carrier(
                &[Carrier::PerRegion, Carrier::Muxed],
                &[Carrier::Muxed, Carrier::PerRegion]
            ),
            Ok(Carrier::PerRegion)
        );
        assert_eq!(
            select_carrier(&[Carrier::Muxed], &[Carrier::PerRegion]),
            Err(CarrierMismatch::NoCommonCarrier)
        );
        assert_eq!(
            select_carrier::<Carrier>(&[], &[Carrier::Muxed]),
            Err(CarrierMismatch::HostOffersNone)
        );
        assert_eq!(
            select_carrier(&[Carrier::Muxed], &[]),
            Err(CarrierMismatch::ClientSupportsNone)
        );
    }

    #[test]
    fn degrade_reasons_convert_from_both_halves() {
        let gate: DegradeReason<&str> = GateClosed::NotAdvertised.into();
        assert_eq!(gate.gate(), Some(GateClosed::NotAdvertised));
        assert!(gate.to_string().contains("never advertised"));

        let rejection: DegradeReason<&str> =
            AdmissionRejection::new(AdmissionStage::Planning, "no head").into();
        assert_eq!(rejection.stage(), Some(AdmissionStage::Planning));
        assert!(rejection.to_string().contains("planning"));
    }
}

#[cfg(test)]
mod region_tests {
    use std::cell::RefCell;

    use arcen_media::{
        Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology, Rotation,
        TopologyGeneration,
    };

    use super::{
        AdmissionGates, AdmissionOutcome, AdmissionStage, DegradeReason, GateClosed,
        RegionAdmissionPolicy, admit_regions,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Carrier {
        Muxed,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Refusal {
        Offer,
        Carrier,
        Request,
        Planning,
        MediaPolicy,
    }

    /// Stands in for a host's wire request sidecar: the driver only ever
    /// hands it back to the policy, so its shape is entirely host business.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Request {
        regions: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Plan {
        regions: usize,
        generation: u64,
    }

    #[derive(Debug, Default)]
    struct Policy {
        refuse: Option<Refusal>,
        steps: RefCell<Vec<&'static str>>,
    }

    impl Policy {
        fn refusing(refusal: Refusal) -> Self {
            Self {
                refuse: Some(refusal),
                steps: RefCell::new(Vec::new()),
            }
        }

        fn note(&self, step: &'static str) {
            self.steps.borrow_mut().push(step);
        }

        fn steps(&self) -> Vec<&'static str> {
            self.steps.borrow().clone()
        }

        fn refuse(&self, refusal: Refusal) -> Result<(), Refusal> {
            if self.refuse == Some(refusal) {
                return Err(refusal);
            }
            Ok(())
        }
    }

    impl RegionAdmissionPolicy for Policy {
        type Request = Request;
        type Carrier = Carrier;
        type Plan = Plan;
        type Rejection = Refusal;

        fn validate_advertised_offer(&self, _requested: &Request) -> Result<(), Refusal> {
            self.note("offer");
            self.refuse(Refusal::Offer)
        }

        fn select_carrier(&self, _requested: &Request) -> Result<Carrier, Refusal> {
            self.note("carrier");
            self.refuse(Refusal::Carrier)?;
            Ok(Carrier::Muxed)
        }

        fn convert_request(
            &self,
            requested: &Request,
        ) -> Result<RequestedMonitorTopology, Refusal> {
            self.note("request");
            self.refuse(Refusal::Request)?;
            let monitors = (0..requested.regions)
                .map(|index| requested_monitor(index, index == 0))
                .collect();
            RequestedMonitorTopology::new(monitors).map_err(|_| Refusal::Request)
        }

        fn plan_topology(
            &self,
            _requested: &Request,
            topology: &RequestedMonitorTopology,
            generation: TopologyGeneration,
        ) -> Result<Plan, Refusal> {
            self.note("planning");
            self.refuse(Refusal::Planning)?;
            Ok(Plan {
                regions: topology.monitors().len(),
                generation: generation.get(),
            })
        }

        fn check_media_policy(&self, _plan: &Plan) -> Result<(), Refusal> {
            self.note("media policy");
            self.refuse(Refusal::MediaPolicy)
        }
    }

    fn requested_monitor(index: usize, primary: bool) -> RequestedMonitor {
        let x = i32::try_from(index).unwrap_or(0) * 1_920;
        RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: format!("display-{index}"),
                    name: format!("Display {index}"),
                    vendor: 0,
                    model: 0,
                    serial: 0,
                },
                x,
                y: 0,
                width_px: 1_920,
                height_px: 1_080,
                scale: 1.0,
                refresh_hz: 60,
                rotation: Rotation::Degrees0,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
            },
            1_920,
            1_080,
        )
        .expect("valid requested monitor")
    }

    #[test]
    fn no_request_runs_no_step_at_all() {
        let policy = Policy::default();
        assert_eq!(
            admit_regions(AdmissionGates::OPEN, &policy, None),
            AdmissionOutcome::NotRequested
        );
        assert!(policy.steps().is_empty());
    }

    #[test]
    fn a_closed_gate_runs_no_step_and_reports_the_first_closed_gate() {
        let policy = Policy::default();
        let outcome = admit_regions(
            AdmissionGates {
                operator_enabled: false,
                ..AdmissionGates::OPEN
            },
            &policy,
            Some(&Request { regions: 1 }),
        );
        assert_eq!(
            outcome.degrade_reason().and_then(DegradeReason::gate),
            Some(GateClosed::OperatorDisabled)
        );
        assert!(policy.steps().is_empty());
    }

    #[test]
    fn every_step_runs_in_the_frozen_order_and_stamps_the_first_generation() {
        let policy = Policy::default();
        let outcome = admit_regions(AdmissionGates::OPEN, &policy, Some(&Request { regions: 2 }));
        assert_eq!(
            outcome,
            AdmissionOutcome::Admitted {
                plan: Plan {
                    regions: 2,
                    generation: TopologyGeneration::FIRST.get(),
                },
                carrier: Carrier::Muxed,
            }
        );
        assert_eq!(
            policy.steps(),
            vec!["offer", "carrier", "request", "planning", "media policy"]
        );
    }

    #[test]
    fn each_step_stops_the_order_and_is_attributed_to_its_own_stage() {
        for (refusal, stage, ran) in [
            (Refusal::Offer, AdmissionStage::Offer, vec!["offer"]),
            (
                Refusal::Carrier,
                AdmissionStage::Carrier,
                vec!["offer", "carrier"],
            ),
            (
                Refusal::Request,
                AdmissionStage::Request,
                vec!["offer", "carrier", "request"],
            ),
            (
                Refusal::Planning,
                AdmissionStage::Planning,
                vec!["offer", "carrier", "request", "planning"],
            ),
            (
                Refusal::MediaPolicy,
                AdmissionStage::MediaPolicy,
                vec!["offer", "carrier", "request", "planning", "media policy"],
            ),
        ] {
            let policy = Policy::refusing(refusal);
            let outcome =
                admit_regions(AdmissionGates::OPEN, &policy, Some(&Request { regions: 1 }));
            let reason = outcome.degrade_reason().expect("degraded");
            assert_eq!(reason.stage(), Some(stage));
            assert_eq!(reason.source(), Some(&refusal));
            assert_eq!(policy.steps(), ran, "steps after {stage} must not run");
        }
    }

    #[test]
    fn a_host_without_a_media_policy_gate_admits_after_planning() {
        struct NoMediaPolicy;

        impl RegionAdmissionPolicy for NoMediaPolicy {
            type Request = Request;
            type Carrier = Carrier;
            type Plan = Plan;
            type Rejection = Refusal;

            fn validate_advertised_offer(&self, _requested: &Request) -> Result<(), Refusal> {
                Ok(())
            }

            fn select_carrier(&self, _requested: &Request) -> Result<Carrier, Refusal> {
                Ok(Carrier::Muxed)
            }

            fn convert_request(
                &self,
                _requested: &Request,
            ) -> Result<RequestedMonitorTopology, Refusal> {
                RequestedMonitorTopology::new(vec![requested_monitor(0, true)])
                    .map_err(|_| Refusal::Request)
            }

            fn plan_topology(
                &self,
                _requested: &Request,
                topology: &RequestedMonitorTopology,
                generation: TopologyGeneration,
            ) -> Result<Plan, Refusal> {
                Ok(Plan {
                    regions: topology.monitors().len(),
                    generation: generation.get(),
                })
            }
        }

        assert!(
            admit_regions(
                AdmissionGates::OPEN,
                &NoMediaPolicy,
                Some(&Request { regions: 1 })
            )
            .is_admitted()
        );
    }
}
