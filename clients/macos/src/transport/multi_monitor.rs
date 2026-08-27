//! Converts the local requested multi-monitor-v1 topology plus a host's
//! pre-auth `AuthRequest.multi_monitor_v1` offer into a validated
//! `AuthResponse.multi_monitor_v1` sidecar.
//!
//! This module intentionally contains no negotiation *policy*: all admission
//! rules (max monitor count, supported rotations, roster invariants, carrier
//! intersection) already live in `arcen_protocol::messages`
//! (`AuthResponse::with_multi_monitor_v1`) and `arcen_media`
//! (`RequestedMonitorTopology`). Deck's job here is strictly (a) translating
//! the domain `RequestedMonitorTopology` into the wire
//! `RequestedMonitorTopologyMsg` shape, (b) applying the explicit host-offer
//! gate: Match My Layout with more than one monitor must fail loudly with
//! [`MultiMonitorAuthError::UnsupportedHost`] rather than silently degrading
//! to a primary-only `AuthResponse`, and (c) advertising exactly the ordered
//! carrier list Deck itself actually implements (see
//! [`deck_supported_carriers`]).

use std::fmt;

use arcen_media::RequestedMonitorTopology;

use crate::protocol::messages::{
    AuthRequest, AuthRequestMultiMonitorOfferError, AuthResponse, MultiMonitorCarrierMsg,
    MultiMonitorValidationError, RequestedMonitorTopologyMsg, SafeAreaPolicyMsg,
};

/// Auth-time multi-monitor-v1 carriers Deck can actually use, in Deck's
/// fixed, deterministic preferred order (index `0` is most preferred).
///
/// This is the single source of truth for what Deck advertises in every
/// [`attach_multi_monitor_v1`] call, so an A/B carrier rollout stays
/// reproducible instead of depending on a `HashSet`/enumeration order or a
/// second ad hoc literal drifting out of sync with this one.
///
/// Today this is exactly one entry: [`MultiMonitorCarrierMsg::MuxedReliableStream`]
/// ("Carrier A"), the existing single reliable stream that already carries
/// every Deck message including video. `arcen_transport`'s direct-monitor
/// stream preface ("Carrier B", [`MultiMonitorCarrierMsg::PerMonitorReliableStream`])
/// is additive shared *transport foundation* only -- no `clients/macos` code
/// opens or accepts a per-monitor QUIC stream yet, so advertising it here
/// would claim a capability Deck cannot actually honor. Add it to this list,
/// in the order Deck should prefer it, only once Deck's connection
/// establishment and media routing actually implement it.
#[must_use]
pub const fn deck_supported_carriers() -> &'static [MultiMonitorCarrierMsg] {
    &[MultiMonitorCarrierMsg::MuxedReliableStream]
}

/// The local requested multi-monitor-v1 topology plus the safe-area policy
/// used to derive its per-monitor stream sizes, threaded from
/// `crate::ui::app::connect_options_with_stream_sizing_policy` into
/// [`crate::transport::websocket::ConnectOptions`] for
/// [`attach_multi_monitor_v1`].
#[derive(Debug, Clone, PartialEq)]
pub struct RequestedMultiMonitorSelection {
    pub topology: RequestedMonitorTopology,
    pub safe_area_policy: SafeAreaPolicyMsg,
    /// Opaque client display IDs that require full-color 4:4:4. When this is
    /// non-empty, unlisted monitors explicitly permit bandwidth-optimized
    /// 4:2:0. Pier still owns backend/GPU assignment.
    pub full_color_display_ids: Vec<String>,
}

/// Failure negotiating the additive multi-monitor-v1 sidecar into an
/// `AuthResponse`.
#[derive(Debug, Clone, PartialEq)]
pub enum MultiMonitorAuthError {
    /// Match My Layout was requested with more than one local display, but
    /// the host's `AuthRequest` did not advertise (or advertised an invalid)
    /// multi-monitor-v1 offer. Callers must surface this instead of silently
    /// downgrading to a primary-only session.
    UnsupportedHost(AuthRequestMultiMonitorOfferError),
    /// The local topology was rejected against the host's advertised offer
    /// (for example, the offer's `max_monitors` is lower than the local
    /// display count, or a requested rotation is unsupported).
    RejectedByAdvertisement(MultiMonitorValidationError),
    /// The local topology could not be translated into the wire shape. This
    /// indicates a mismatch between the `arcen_media` and `arcen_protocol`
    /// validation rules and should not happen for a topology that already
    /// passed local preflight.
    WireConversionFailed(MultiMonitorValidationError),
}

impl fmt::Display for MultiMonitorAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedHost(error) => write!(
                formatter,
                "Match My Layout needs more than one display, but this host does not support \
                 multi-monitor-v1: {error}"
            ),
            Self::RejectedByAdvertisement(error) => write!(
                formatter,
                "requested display layout was rejected by the host's multi-monitor-v1 offer: {error}"
            ),
            Self::WireConversionFailed(error) => write!(
                formatter,
                "requested display layout could not be encoded for the host: {error}"
            ),
        }
    }
}

impl std::error::Error for MultiMonitorAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsupportedHost(error) => Some(error),
            Self::RejectedByAdvertisement(error) | Self::WireConversionFailed(error) => Some(error),
        }
    }
}

/// Converts a validated local domain topology into the wire message shape.
///
/// # Errors
///
/// Returns [`MultiMonitorAuthError::WireConversionFailed`] when the wire-level
/// invariants (a superset that additionally covers the legacy numeric monitor
/// id) reject a topology that already passed local `arcen_media` validation.
pub fn to_wire_topology(
    topology: &RequestedMonitorTopology,
    safe_area_policy: SafeAreaPolicyMsg,
) -> Result<RequestedMonitorTopologyMsg, MultiMonitorAuthError> {
    to_wire_topology_with_quality(topology, safe_area_policy, &[])
}

pub fn to_wire_topology_with_quality(
    topology: &RequestedMonitorTopology,
    safe_area_policy: SafeAreaPolicyMsg,
    full_color_display_ids: &[String],
) -> Result<RequestedMonitorTopologyMsg, MultiMonitorAuthError> {
    let mut monitors = Vec::with_capacity(topology.monitors().len());
    for requested in topology.monitors() {
        let quality_intent = if full_color_display_ids
            .iter()
            .any(|id| id == &requested.monitor().identity.id)
        {
            crate::protocol::messages::MonitorQualityIntentMsg::FullColorRequired
        } else {
            crate::protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized
        };
        let descriptor = requested
            .to_wire_descriptor(safe_area_policy, quality_intent)
            .map_err(MultiMonitorAuthError::WireConversionFailed)?;
        monitors.push(descriptor);
    }
    RequestedMonitorTopologyMsg::new(monitors).map_err(MultiMonitorAuthError::WireConversionFailed)
}

/// Attaches the validated multi-monitor-v1 sidecar to `response`, requiring
/// the host to have advertised a valid pre-auth offer.
///
/// # Errors
///
/// Returns [`MultiMonitorAuthError::UnsupportedHost`] when `auth_request` did
/// not advertise a valid multi-monitor-v1 offer,
/// [`MultiMonitorAuthError::WireConversionFailed`] when the local topology
/// cannot be encoded, and [`MultiMonitorAuthError::RejectedByAdvertisement`]
/// when the host's offer rejects the local topology (for example an
/// unsupported rotation or a monitor count above the offer's `max_monitors`).
pub fn attach_multi_monitor_v1(
    response: AuthResponse,
    auth_request: &AuthRequest,
    local_topology: &RequestedMonitorTopology,
    safe_area_policy: SafeAreaPolicyMsg,
) -> Result<AuthResponse, MultiMonitorAuthError> {
    attach_multi_monitor_v1_with_quality(
        response,
        auth_request,
        local_topology,
        safe_area_policy,
        &[],
    )
}

pub fn attach_multi_monitor_v1_with_quality(
    response: AuthResponse,
    auth_request: &AuthRequest,
    local_topology: &RequestedMonitorTopology,
    safe_area_policy: SafeAreaPolicyMsg,
    full_color_display_ids: &[String],
) -> Result<AuthResponse, MultiMonitorAuthError> {
    let offer = auth_request
        .required_multi_monitor_v1_offer()
        .map_err(MultiMonitorAuthError::UnsupportedHost)?;
    let wire_topology =
        to_wire_topology_with_quality(local_topology, safe_area_policy, full_color_display_ids)?;
    response
        .with_multi_monitor_v1(offer, wire_topology, deck_supported_carriers().to_vec())
        .map_err(MultiMonitorAuthError::RejectedByAdvertisement)
}

/// Whether an `AuthResponse` should carry the multi-monitor-v1 sidecar for the
/// given local display count and Match My Layout selection. Primary-only and
/// windowed selections always stay on the legacy-compatible `with_displays`
/// path, and so does Match My Layout with a single display (there is nothing
/// additive to negotiate).
#[must_use]
pub const fn wants_multi_monitor(match_layout_selected: bool, local_display_count: usize) -> bool {
    match_layout_selected && local_display_count > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::{
        AuthMultiMonitorOfferMsg, MultiMonitorCarrierMsg, RotationMsg,
    };
    use arcen_media::{Monitor, MonitorIdentity, RequestedMonitor, Rotation};

    fn monitor(id: &str, primary: bool, x: i32) -> Monitor {
        Monitor {
            identity: MonitorIdentity {
                id: id.to_string(),
                name: format!("Display {id}"),
                vendor: 1,
                model: 2,
                serial: 3,
            },
            x,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 2.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary,
            width_mm: 300.0,
            height_mm: 200.0,
        }
    }

    fn requested(id: &str, primary: bool, x: i32) -> RequestedMonitor {
        RequestedMonitor::new(monitor(id, primary, x), 960, 540).expect("valid requested monitor")
    }

    fn topology(count: usize) -> RequestedMonitorTopology {
        let monitors = (0..count)
            .map(|i| requested(&(1000 + i as u32).to_string(), i == 0, i as i32 * 960))
            .collect();
        RequestedMonitorTopology::new(monitors).expect("valid topology")
    }

    fn offer(max_monitors: u8) -> AuthMultiMonitorOfferMsg {
        offer_with_carriers(
            max_monitors,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
    }

    fn offer_with_carriers(
        max_monitors: u8,
        carriers: Vec<MultiMonitorCarrierMsg>,
    ) -> AuthMultiMonitorOfferMsg {
        AuthMultiMonitorOfferMsg::new(max_monitors, vec![RotationMsg::Degrees0], carriers)
            .expect("valid offer")
    }

    fn bare_auth_request() -> AuthRequest {
        AuthRequest {
            msg_type: crate::protocol::messages::AUTH_REQUEST.to_string(),
            auth_methods: vec!["password".to_string()],
            challenge: "challenge".to_string(),
            salt: String::new(),
            auth_mode: None,
            disclaimer: None,
            multi_monitor_v1: None,
        }
    }

    #[test]
    fn wants_multi_monitor_requires_match_layout_and_more_than_one_display() {
        assert!(!wants_multi_monitor(false, 4));
        assert!(!wants_multi_monitor(true, 1));
        assert!(wants_multi_monitor(true, 2));
        assert!(wants_multi_monitor(true, 4));
    }

    #[test]
    fn wire_round_trip_preserves_monitor_count_and_primary() {
        let local = topology(2);
        let wire = to_wire_topology(&local, SafeAreaPolicyMsg::StandardFullscreen)
            .expect("conversion succeeds");
        assert_eq!(wire.monitors().len(), 2);
        assert!(wire.primary().is_primary);
    }

    #[test]
    fn missing_host_offer_is_unsupported_host_not_silent_downgrade() {
        let auth_request = bare_auth_request();
        let response = AuthResponse::password("user", "pass");
        let local = topology(2);
        let error = attach_multi_monitor_v1(
            response,
            &auth_request,
            &local,
            SafeAreaPolicyMsg::StandardFullscreen,
        )
        .unwrap_err();
        assert!(matches!(error, MultiMonitorAuthError::UnsupportedHost(_)));
    }

    #[test]
    fn host_offer_present_produces_match_layout_response() {
        let auth_request = bare_auth_request()
            .with_multi_monitor_v1_offer(offer(4))
            .expect("valid offer attaches");
        let response = AuthResponse::password("user", "pass");
        let local = topology(4);
        let response = attach_multi_monitor_v1(
            response,
            &auth_request,
            &local,
            SafeAreaPolicyMsg::StandardFullscreen,
        )
        .expect("negotiation succeeds");
        assert_eq!(response.displays_mode, "match_layout");
        assert_eq!(response.monitors.len(), 4);
        assert!(response.multi_monitor_v1.is_some());
    }

    #[test]
    fn host_offer_below_local_count_is_rejected_not_downgraded() {
        let auth_request = bare_auth_request()
            .with_multi_monitor_v1_offer(offer(2))
            .expect("valid offer attaches");
        let response = AuthResponse::password("user", "pass");
        let local = topology(4);
        let error = attach_multi_monitor_v1(
            response,
            &auth_request,
            &local,
            SafeAreaPolicyMsg::StandardFullscreen,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MultiMonitorAuthError::RejectedByAdvertisement(_)
        ));
    }

    #[test]
    fn primary_only_never_needs_multi_monitor_negotiation() {
        assert!(!wants_multi_monitor(false, 4));
    }

    #[test]
    fn deck_supported_carriers_is_nonempty_and_deterministic() {
        let first = deck_supported_carriers();
        let second = deck_supported_carriers();
        assert!(
            !first.is_empty(),
            "Deck must always advertise at least one carrier it actually implements"
        );
        assert_eq!(
            first, second,
            "the advertised carrier order must be stable across calls for reproducible A/B config"
        );
    }

    #[test]
    fn deck_supported_carriers_only_claims_the_muxed_reliable_stream_it_actually_implements() {
        // `clients/macos` does not open or accept a per-monitor QUIC stream
        // (Carrier B / `PerMonitorReliableStream`) anywhere today -- only
        // shared `arcen_transport` foundation exists for it. Advertising it
        // here would claim a capability Deck cannot actually honor.
        assert_eq!(
            deck_supported_carriers(),
            &[MultiMonitorCarrierMsg::MuxedReliableStream]
        );
    }

    #[test]
    fn attach_multi_monitor_v1_advertises_exactly_deck_supported_carriers() {
        let auth_request = bare_auth_request()
            .with_multi_monitor_v1_offer(offer(4))
            .expect("valid offer attaches");
        let response = AuthResponse::password("user", "pass");
        let local = topology(4);
        let response = attach_multi_monitor_v1(
            response,
            &auth_request,
            &local,
            SafeAreaPolicyMsg::StandardFullscreen,
        )
        .expect("negotiation succeeds");
        let attached = response
            .multi_monitor_v1
            .expect("sidecar attached")
            .carriers()
            .to_vec();
        assert_eq!(attached, deck_supported_carriers().to_vec());
    }

    #[test]
    fn selected_display_requires_444_and_unselected_displays_allow_420() {
        let auth_request = bare_auth_request()
            .with_multi_monitor_v1_offer(offer(4))
            .expect("valid offer attaches");
        let local = topology(2);
        let selected = local.monitors()[0].monitor().identity.id.clone();
        let response = attach_multi_monitor_v1_with_quality(
            AuthResponse::password("user", "pass"),
            &auth_request,
            &local,
            SafeAreaPolicyMsg::StandardFullscreen,
            &[selected],
        )
        .expect("negotiation succeeds");
        let sidecar = response.multi_monitor_v1.expect("sidecar");
        let requested = sidecar.requested_topology();
        assert_eq!(
            requested.monitors()[0].quality_intent,
            crate::protocol::messages::MonitorQualityIntentMsg::FullColorRequired
        );
        assert_eq!(
            requested.monitors()[1].quality_intent,
            crate::protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized
        );
    }

    #[test]
    fn no_selected_display_allows_bandwidth_optimized_420() {
        let local = topology(2);
        let wire =
            to_wire_topology_with_quality(&local, SafeAreaPolicyMsg::StandardFullscreen, &[])
                .expect("wire topology");
        assert!(wire.monitors().iter().all(|monitor| {
            monitor.quality_intent
                == crate::protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized
        }));
    }

    #[test]
    fn no_common_carrier_between_deck_and_host_is_rejected_not_downgraded() {
        // The host only offers Carrier B, which Deck does not implement
        // (`deck_supported_carriers` is Carrier A only), so the auth-time
        // carrier intersection must be empty and the connection must fail
        // loudly rather than silently negotiating something Deck cannot
        // honor.
        let auth_request = bare_auth_request()
            .with_multi_monitor_v1_offer(offer_with_carriers(
                4,
                vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
            ))
            .expect("valid offer attaches");
        let response = AuthResponse::password("user", "pass");
        let local = topology(2);
        let error = attach_multi_monitor_v1(
            response,
            &auth_request,
            &local,
            SafeAreaPolicyMsg::StandardFullscreen,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MultiMonitorAuthError::RejectedByAdvertisement(_)
        ));
    }
}
