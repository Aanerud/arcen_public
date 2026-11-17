// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
//! to a primary-only `AuthResponse`, (b2) asking for a monitor upright at its
//! as-seen size when the host does not rotate it
//! ([`upright_unsupported_rotations`]), and (c) advertising exactly the ordered
//! carrier list Deck itself actually implements (see
//! [`deck_supported_carriers`]).

#![allow(
    missing_debug_implementations,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::disallowed_methods,
    clippy::expect_used,
    clippy::large_enum_variant,
    clippy::large_futures,
    clippy::manual_let_else,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_errors_doc,
    clippy::missing_fields_in_debug,
    clippy::needless_pass_by_value,
    clippy::redundant_else,
    clippy::result_large_err,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref,
    clippy::type_complexity,
    clippy::unnecessary_wraps,
    clippy::unwrap_used,
    clippy::assigning_clones,
    clippy::needless_continue,
    reason = "the session engine moved verbatim from the macOS Deck (ADR 0024 step 4); \
              these lints are tightened in follow-ups, never by changing behaviour in the move"
)]

use std::fmt;

use arcen_media::RequestedMonitorTopology;

use arcen_protocol::messages::{
    AuthRequest, AuthRequestMultiMonitorOfferError, AuthResponse, MultiMonitorCarrierMsg,
    MultiMonitorValidationError, RequestedMonitorDescriptorMsg, RequestedMonitorTopologyMsg,
    RotationMsg, SafeAreaPolicyMsg,
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
/// the macOS Deck's `ui::app::connect_options_with_stream_sizing_policy` into
/// [`crate::runtime::session::ConnectOptions`] for
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
    /// display count). A rotation the offer does not list is asked for
    /// upright instead ([`upright_unsupported_rotations`]).
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
    let monitors = wire_descriptors(topology, safe_area_policy, full_color_display_ids)?;
    RequestedMonitorTopologyMsg::new(monitors).map_err(MultiMonitorAuthError::WireConversionFailed)
}

fn wire_descriptors(
    topology: &RequestedMonitorTopology,
    safe_area_policy: SafeAreaPolicyMsg,
    full_color_display_ids: &[String],
) -> Result<Vec<RequestedMonitorDescriptorMsg>, MultiMonitorAuthError> {
    let mut monitors = Vec::with_capacity(topology.monitors().len());
    for requested in topology.monitors() {
        let quality_intent = if full_color_display_ids
            .iter()
            .any(|id| id == &requested.monitor().identity.id)
        {
            arcen_protocol::messages::MonitorQualityIntentMsg::FullColorRequired
        } else {
            arcen_protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized
        };
        let descriptor = requested
            .to_wire_descriptor(safe_area_policy, quality_intent)
            .map_err(MultiMonitorAuthError::WireConversionFailed)?;
        monitors.push(descriptor);
    }
    Ok(monitors)
}

/// Asks for every monitor whose rotation the host's offer does not list as
/// an upright monitor (`Degrees0`), and returns each one changed with the
/// rotation it had.
///
/// A Deck states every monitor as it is seen on the desk
/// (`TransformConvention::AlreadyCompositorOriented`): a quarter-turned
/// monitor's `width_px`/`height_px`, logical size and physical millimetres
/// are already its portrait extent, and `rotation` only tells a host that
/// can drive a rotated panel to do so. A host that cannot (Wayland virtual
/// outputs, Windows capture without IDD, a Linux head without rotation)
/// serves that same as-seen size upright, which the Deck presents
/// unchanged in that monitor's window. Nothing else in the descriptor
/// changes, so positions, the primary and input mapping stay exactly as
/// requested. A rotation is never dropped when the offer does not list
/// `Degrees0` either; the shared validation then reports it.
pub fn upright_unsupported_rotations(
    monitors: &mut [RequestedMonitorDescriptorMsg],
    supported_rotations: &[RotationMsg],
) -> Vec<(String, RotationMsg)> {
    if !supported_rotations.contains(&RotationMsg::Degrees0) {
        return Vec::new();
    }
    let mut changed = Vec::new();
    for monitor in monitors {
        if !supported_rotations.contains(&monitor.rotation) {
            changed.push((
                monitor.client_display_id.as_str().to_owned(),
                monitor.rotation,
            ));
            monitor.rotation = RotationMsg::Degrees0;
        }
    }
    changed
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
/// when the host's offer rejects the local topology (for example a monitor
/// count above the offer's `max_monitors`). A monitor whose rotation the
/// offer does not list is requested upright at its as-seen size
/// ([`upright_unsupported_rotations`]); rotation alone never fails the layout.
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
    let mut monitors = wire_descriptors(local_topology, safe_area_policy, full_color_display_ids)?;
    for (client_display_id, rotation) in
        upright_unsupported_rotations(&mut monitors, offer.supported_rotations())
    {
        tracing::info!(
            target: crate::log_target::SESSION,
            client_display_id,
            rotation = %rotation,
            "the host does not rotate this monitor; it is requested upright at the size it is seen on the desk",
        );
    }
    let wire_topology = RequestedMonitorTopologyMsg::new(monitors)
        .map_err(MultiMonitorAuthError::WireConversionFailed)?;
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
    use arcen_media::{Monitor, MonitorIdentity, RequestedMonitor, Rotation};
    use arcen_protocol::messages::{AuthMultiMonitorOfferMsg, MultiMonitorCarrierMsg, RotationMsg};

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
            color: None,
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
            msg_type: arcen_protocol::messages::AUTH_REQUEST.to_string(),
            auth_methods: vec!["password".to_string()],
            challenge: "challenge".to_string(),
            salt: String::new(),
            auth_mode: None,
            disclaimer: None,
            multi_monitor_v1: None,
        }
    }

    /// The owner's desk: a 2560x1440 primary, a 1440x2560 monitor turned
    /// 270 degrees above-left of it, and a 1080x1920 one turned 90 degrees
    /// on its right, each stated as seen (portrait size and millimetres).
    fn turned_desk() -> RequestedMonitorTopology {
        let at = |id: &str, primary, (x, y), (width, height), (width_mm, height_mm), rotation| {
            let mut monitor = monitor(id, primary, x);
            monitor.y = y;
            monitor.width_px = width;
            monitor.height_px = height;
            monitor.scale = 1.0;
            monitor.width_mm = width_mm;
            monitor.height_mm = height_mm;
            monitor.rotation = rotation;
            RequestedMonitor::new(monitor, width, height).expect("valid requested monitor")
        };
        RequestedMonitorTopology::new(vec![
            at(
                "2",
                true,
                (0, 0),
                (2560, 1440),
                (599.0, 340.0),
                Rotation::Degrees0,
            ),
            at(
                "3",
                false,
                (-1440, -1120),
                (1440, 2560),
                (340.0, 599.0),
                Rotation::Degrees270,
            ),
            at(
                "4",
                false,
                (2560, 0),
                (1080, 1920),
                (300.0, 530.0),
                Rotation::Degrees90,
            ),
        ])
        .expect("valid topology")
    }

    fn negotiate_turned_desk(
        supported_rotations: Vec<RotationMsg>,
    ) -> Result<AuthResponse, MultiMonitorAuthError> {
        let offer = AuthMultiMonitorOfferMsg::new(
            4,
            supported_rotations,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("valid offer");
        let auth_request = bare_auth_request()
            .with_multi_monitor_v1_offer(offer)
            .expect("valid offer attaches");
        attach_multi_monitor_v1(
            AuthResponse::password("user", "pass"),
            &auth_request,
            &turned_desk(),
            SafeAreaPolicyMsg::StandardFullscreen,
        )
    }

    /// `(id, rotation, x, y, width, height, width_mm, height_mm, primary)`
    /// of each requested monitor, in layout order.
    fn requested_facts(
        response: &AuthResponse,
    ) -> Vec<(String, RotationMsg, i32, i32, u32, u32, f32, f32, bool)> {
        response
            .multi_monitor_v1
            .as_ref()
            .expect("sidecar attached")
            .requested_topology()
            .monitors()
            .iter()
            .map(|monitor| {
                (
                    monitor.client_display_id.as_str().to_owned(),
                    monitor.rotation,
                    monitor.x,
                    monitor.y,
                    monitor.width_px,
                    monitor.height_px,
                    monitor.width_mm,
                    monitor.height_mm,
                    monitor.is_primary,
                )
            })
            .collect()
    }

    #[test]
    fn a_host_that_does_not_rotate_gets_turned_monitors_upright_at_their_as_seen_size() {
        let response = negotiate_turned_desk(vec![RotationMsg::Degrees0])
            .expect("rotation alone never fails the layout");
        assert_eq!(
            requested_facts(&response),
            vec![
                (
                    "2".into(),
                    RotationMsg::Degrees0,
                    0,
                    0,
                    2560,
                    1440,
                    599.0,
                    340.0,
                    true
                ),
                (
                    "3".into(),
                    RotationMsg::Degrees0,
                    -1440,
                    -1120,
                    1440,
                    2560,
                    340.0,
                    599.0,
                    false
                ),
                (
                    "4".into(),
                    RotationMsg::Degrees0,
                    2560,
                    0,
                    1080,
                    1920,
                    300.0,
                    530.0,
                    false
                ),
            ],
            "only the rotation changes: sizes, millimetres, positions and the primary stay as seen"
        );
        assert!(
            response
                .monitors
                .iter()
                .all(|monitor| monitor.rotation_degrees == 0),
            "the legacy roster agrees with the sidecar"
        );
        let offer = AuthMultiMonitorOfferMsg::new(
            4,
            vec![RotationMsg::Degrees0],
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
        )
        .expect("valid offer");
        arcen_protocol::messages::AdvertisedMultiMonitorOffer::from_offer(&offer)
            .expect("valid offer")
            .validate_request(response.multi_monitor_v1.as_ref().expect("sidecar"))
            .expect("the host's own check accepts the request");
    }

    #[test]
    fn a_host_that_rotates_receives_every_rotation() {
        let response =
            negotiate_turned_desk(RotationMsg::ALL.to_vec()).expect("negotiation succeeds");
        let rotations: Vec<_> = requested_facts(&response)
            .into_iter()
            .map(|facts| (facts.0, facts.1, facts.4, facts.5))
            .collect();
        assert_eq!(
            rotations,
            vec![
                ("2".into(), RotationMsg::Degrees0, 2560, 1440),
                ("3".into(), RotationMsg::Degrees270, 1440, 2560),
                ("4".into(), RotationMsg::Degrees90, 1080, 1920),
            ]
        );
        assert_eq!(
            response
                .monitors
                .iter()
                .map(|monitor| monitor.rotation_degrees)
                .collect::<Vec<_>>(),
            vec![0, 270, 90]
        );
    }

    #[test]
    fn only_the_rotations_a_host_lacks_are_requested_upright() {
        let response = negotiate_turned_desk(vec![RotationMsg::Degrees0, RotationMsg::Degrees90])
            .expect("negotiation succeeds");
        let rotations: Vec<_> = requested_facts(&response)
            .into_iter()
            .map(|facts| (facts.0, facts.1))
            .collect();
        assert_eq!(
            rotations,
            vec![
                ("2".into(), RotationMsg::Degrees0),
                ("3".into(), RotationMsg::Degrees0),
                ("4".into(), RotationMsg::Degrees90),
            ]
        );
    }

    #[test]
    fn an_offer_without_upright_keeps_the_rotation_for_the_shared_check_to_report() {
        let error = negotiate_turned_desk(vec![RotationMsg::Degrees90]).unwrap_err();
        assert!(matches!(
            error,
            MultiMonitorAuthError::RejectedByAdvertisement(_)
        ));
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
            .as_ref()
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
        let sidecar = response.multi_monitor_v1.as_ref().expect("sidecar");
        let requested = sidecar.requested_topology();
        assert_eq!(
            requested.monitors()[0].quality_intent,
            arcen_protocol::messages::MonitorQualityIntentMsg::FullColorRequired
        );
        assert_eq!(
            requested.monitors()[1].quality_intent,
            arcen_protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized
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
                == arcen_protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized
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
