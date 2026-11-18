// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's displays in a connection (owner: `ui` stream), by the macOS
//! Deck's rules, which live in `arcen_deck_core`: the Displays mode an
//! attempt uses (`connect_plan::resolve_attempt_displays_mode`), Match My
//! Layout's preflight and requested topology (`connect_plan::
//! match_layout_preflight`, `display::topology`), the pinned stream size
//! (`display::primary_presentation_size`, `HiDPI` per display with the
//! EDID fallback), the fit-to-window updates of a Windowed session
//! (`display_fit`), the fullscreen a pinned session holds
//! (`display::SessionFullscreen`) and the reconnect identity's display
//! state. What is here only enumerates Windows' monitors
//! ([`crate::display::enumerate_with_metrics`]) and hands them over.

use arcen_deck_core::connect_plan::{
    StreamSizingPolicy, apply_remote_ui_scale_to_requested_topology,
    approved_multi_monitor_selection, client_monitors_from_topology, full_color_display_ids,
    legacy_roster_for_connect_options, match_layout_preflight, primary_monitor,
};
use arcen_deck_core::display::topology::build_requested_topology_from;
use arcen_deck_core::display::{DisplayMetrics, primary_presentation_size};
use arcen_deck_core::runtime::multi_monitor::RequestedMultiMonitorSelection;
use arcen_deck_core::runtime::session::ConnectOptions;
use arcen_deck_core::screens::settings::SettingsMonitor;
use arcen_deck_core::settings::{ClientSettings, DisplaysMode, RemoteUiScale};
use arcen_protocol::messages::{ClientMonitor, SafeAreaPolicyMsg};

/// Whether this Deck can present a multi-monitor layout (one window per
/// display): yes, as on the Mac.
pub(super) const MULTI_WINDOW_RUNTIME_AVAILABLE: bool = true;

/// What a connection's display plan decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayPlan {
    /// Following the window, or pinned to the primary display.
    pub policy: StreamSizingPolicy,
    /// The display (`\\.\DISPLAYn`'s `n`) a pinned session's window is
    /// fullscreen on: the one the stream was sized for.
    pub pinned_display: Option<u32>,
}

/// This PC's displays now: the roster, primary first, each with its
/// metrics.
fn displays() -> Vec<(ClientMonitor, DisplayMetrics)> {
    crate::display::enumerate_with_metrics()
}

/// Match My Layout's preflight against this PC's displays: the one
/// validated topology this attempt asks for, with its full-colour roster;
/// `Ok(None)` when the mode or a single display needs none. Windows has no
/// local prerequisite (the Mac's Separate Spaces) and no notch, so neither
/// gate applies.
///
/// # Errors
///
/// The user-facing reason the attempt stops before anything is sent.
pub(super) fn approve_match_layout(
    mode: DisplaysMode,
    settings: &ClientSettings,
) -> Result<Option<RequestedMultiMonitorSelection>, String> {
    #[cfg(feature = "dev-tools")]
    if let Some(layout) = super::windowed_test::layout() {
        let mut selection = super::windowed_test::selection(layout)?;
        selection.full_color_display_ids = full_color_display_ids(
            &selection.topology,
            settings.color_fidelity,
            settings.full_color_display_mask,
        );
        return Ok(Some(selection));
    }
    approve_match_layout_with(mode, settings, &displays())
}

fn approve_match_layout_with(
    mode: DisplaysMode,
    settings: &ClientSettings,
    displays: &[(ClientMonitor, DisplayMetrics)],
) -> Result<Option<RequestedMultiMonitorSelection>, String> {
    let mut approved = match_layout_preflight(
        mode,
        displays.len(),
        |_| Ok(()),
        None,
        || build_requested_topology_from(displays, false, settings.hidpi_display_mask),
        MULTI_WINDOW_RUNTIME_AVAILABLE,
    )?;
    if let Some(selection) = approved.as_mut() {
        selection.full_color_display_ids = full_color_display_ids(
            &selection.topology,
            settings.color_fidelity,
            settings.full_color_display_mask,
        );
    }
    Ok(approved)
}

/// Fills `options`' displays for `mode` (the Mac's
/// `connect_options_with_stream_sizing_policy`): the approved topology
/// scaled to the Remote UI Scale, the legacy roster that matches it, and
/// the pinned stream size.
pub(super) fn apply_display_plan(
    options: &mut ConnectOptions,
    settings: &ClientSettings,
    mode: DisplaysMode,
    approved: Option<RequestedMultiMonitorSelection>,
) -> DisplayPlan {
    // A simulated layout (developer builds) asks for its own monitors, never
    // this PC's, and its windows are never fullscreen.
    #[cfg(feature = "dev-tools")]
    let approved = match (super::windowed_test::layout(), approved) {
        (Some(_), Some(selection)) => {
            let monitors = client_monitors_from_topology(&selection.topology);
            options.monitors.clone_from(&monitors);
            options.multi_monitor_topology = Some(selection);
            mode.as_wire().clone_into(&mut options.displays_mode);
            return DisplayPlan {
                policy: StreamSizingPolicy::resolve(mode, &monitors, None, true),
                pinned_display: None,
            };
        }
        (_, approved) => approved,
    };
    apply_display_plan_with(options, settings, mode, approved, &displays())
}

fn apply_display_plan_with(
    options: &mut ConnectOptions,
    settings: &ClientSettings,
    mode: DisplaysMode,
    approved: Option<RequestedMultiMonitorSelection>,
    displays: &[(ClientMonitor, DisplayMetrics)],
) -> DisplayPlan {
    let monitors: Vec<ClientMonitor> = displays
        .iter()
        .map(|(monitor, _)| monitor.clone())
        .collect();
    // The primary-only path streams one display: mask slot 0.
    let presentation =
        primary_presentation_size(displays, false, settings.hidpi_display_mask & 0b0001 != 0);
    let remote_ui_scale = RemoteUiScale::from_percent(settings.remote_ui_scale_percent);
    let approved = approved.map(|selection| RequestedMultiMonitorSelection {
        topology: apply_remote_ui_scale_to_requested_topology(
            &selection.topology,
            remote_ui_scale,
            settings.hidpi_display_mask,
        ),
        safe_area_policy: selection.safe_area_policy,
        full_color_display_ids: selection.full_color_display_ids,
    });
    options.multi_monitor_topology =
        approved_multi_monitor_selection(mode, approved, MULTI_WINDOW_RUNTIME_AVAILABLE);
    let policy = StreamSizingPolicy::resolve(
        mode,
        &monitors,
        presentation,
        options.multi_monitor_topology.is_some(),
    );
    options.monitors = legacy_roster_for_connect_options(
        options.multi_monitor_topology.as_ref(),
        policy,
        &monitors,
        remote_ui_scale,
        settings.hidpi_display_mask,
    );
    mode.as_wire().clone_into(&mut options.displays_mode);
    let pinned_display = policy
        .is_pinned()
        .then(|| primary_monitor(&monitors).map(|monitor| monitor.id))
        .flatten();
    // The displays' own refresh (a 144 Hz panel's 144), which hosts follow;
    // the requested modes above stay pinned. A pinned session is shown on
    // its pinned display, a windowed one on the primary.
    options.display_refresh =
        arcen_deck_core::connect_plan::deck_display_refresh(&monitors, pinned_display);
    if policy.is_pinned() {
        tracing::info!(
            target: "arcen::display",
            mode = mode.as_wire(),
            panel = ?primary_monitor(&monitors).map(|m| [m.width_px, m.height_px]),
            requested = ?policy.primary_size(),
            display = ?pinned_display,
            multi_monitor = options.multi_monitor_topology.is_some(),
            "pinned session sized to the fullscreen presentation area",
        );
    }
    DisplayPlan {
        policy,
        pinned_display,
    }
}

/// The display state a reconnect identity hashes (the Mac's
/// `connection_identity` and `connection_identity_baseline`): the
/// negotiated topology as frozen in `options` for the baseline of a
/// multi-monitor session, otherwise this PC's displays now, scaled and
/// reduced to what was negotiated.
pub(super) fn identity_display_state(
    options: &ConnectOptions,
    settings: &ClientSettings,
    baseline: bool,
) -> (
    Vec<ClientMonitor>,
    Option<arcen_media::RequestedMonitorTopology>,
    SafeAreaPolicyMsg,
) {
    if baseline && let Some(selection) = options.multi_monitor_topology.as_ref() {
        return (
            client_monitors_from_topology(&selection.topology),
            Some(selection.topology.clone()),
            selection.safe_area_policy,
        );
    }
    let displays = displays();
    let roster: Vec<ClientMonitor> = displays
        .iter()
        .map(|(monitor, _)| monitor.clone())
        .collect();
    let topology =
        build_requested_topology_from(&displays, false, settings.hidpi_display_mask).ok();
    let (monitors, topology) = arcen_deck_core::connect_plan::identity_display_state(
        roster,
        topology,
        settings.remote_ui_scale_percent,
        settings.hidpi_display_mask,
        options.multi_monitor_topology.is_some(),
    );
    (monitors, topology, SafeAreaPolicyMsg::StandardFullscreen)
}

/// Settings › Displays: each display with what a fullscreen session
/// streams on it with `HiDPI` streaming off and on.
pub(super) fn settings_monitors() -> Vec<SettingsMonitor> {
    displays()
        .into_iter()
        .map(|(monitor, metrics)| SettingsMonitor {
            monitor,
            fullscreen_size: [false, true]
                .map(|hidpi| Some(metrics.presentation_stream_extent(false, hidpi).as_array())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;
    use crate::display::{WindowsDisplayFacts, roster_with_metrics};

    /// A Windows monitor at physical `(left, top)`, `width`×`height` pixels
    /// and `dpi`.
    fn monitor(
        id: u32,
        left: i32,
        width_px: u32,
        height_px: u32,
        dpi: u32,
        primary: bool,
    ) -> WindowsDisplayFacts {
        WindowsDisplayFacts {
            gdi_name: format!(r"\\.\DISPLAY{id}"),
            id,
            left,
            top: 0,
            width_px,
            height_px,
            dpi,
            primary,
            refresh_hz: Some(60.0),
            rotation: arcen_media::Rotation::Degrees0,
            friendly_name: Some(format!("Test {id}")),
            edid_manufacturer: 0,
            edid_product: 0,
            advanced_color_supported: None,
            advanced_color_enabled: None,
            wide_color_enforced: None,
            advanced_color_force_disabled: None,
            color_encoding: None,
            bits_per_color_channel: None,
            dxgi: None,
            sdr_white_nits: None,
        }
    }

    /// A common two-monitor desk: a 4K panel at 150 % with a 1440p panel at
    /// 100 % to its right.
    fn two_monitors() -> Vec<(ClientMonitor, DisplayMetrics)> {
        roster_with_metrics(&[
            monitor(1, 0, 3840, 2160, 144, true),
            monitor(2, 3840, 2560, 1440, 96, false),
        ])
    }

    fn options() -> ConnectOptions {
        ConnectOptions {
            display_refresh: None,
            services: Arc::new(crate::platform::WindowsDeckServices::new()),
            host: "pier.example.internal".to_owned(),
            port: 18444,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: std::time::Duration::from_secs(30),
            tls: arcen_deck_core::runtime::tls::TlsTrustConfig::default(),
            profile: arcen_deck_core::runtime::session::StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: ClientSettings::default().cursor_preference,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: false,
            tablet_mode_requested: ClientSettings::default().tablet_mode_requested,
            telemetry: Arc::new(arcen_deck_core::client_telemetry::ClientTelemetry::default()),
            exact_still_image: false,
        }
    }

    #[test]
    fn match_my_layout_asks_for_every_display_side_by_side() {
        let settings = ClientSettings::default();
        let displays = two_monitors();
        let approved = approve_match_layout_with(DisplaysMode::MatchLayout, &settings, &displays)
            .expect("two displays pass the preflight")
            .expect("two displays need a layout");
        let monitors = approved.topology.monitors();
        assert_eq!(monitors.len(), 2);
        // Point-size streams by default; the 1440p panel touches the 4K
        // panel's logical right edge.
        let first = monitors[0].monitor();
        let second = monitors[1].monitor();
        assert!(first.primary);
        assert_eq!((first.width_px, first.height_px), (2560, 1440));
        assert_eq!(
            (second.x, second.width_px, second.height_px),
            (2560, 2560, 1440)
        );
        assert_eq!(
            approved.safe_area_policy,
            SafeAreaPolicyMsg::StandardFullscreen
        );

        let mut options = options();
        let plan = apply_display_plan_with(
            &mut options,
            &settings,
            DisplaysMode::MatchLayout,
            Some(approved),
            &displays,
        );
        assert!(plan.policy.is_pinned());
        assert!(!plan.policy.match_layout_fallback());
        assert!(options.multi_monitor_topology.is_some());
        // The legacy roster is the same snapshot as the layout.
        assert_eq!(options.monitors.len(), 2);
        assert_eq!(options.displays_mode, "match_layout");
    }

    #[test]
    fn full_resolution_streams_a_scaled_panels_own_pixels_one_to_one() {
        let settings = ClientSettings {
            hidpi_display_mask: 0b0001,
            ..ClientSettings::default()
        };
        let displays = two_monitors();
        let approved = approve_match_layout_with(DisplaysMode::MatchLayout, &settings, &displays)
            .unwrap()
            .unwrap();
        let first = approved.topology.monitors()[0].monitor().clone();
        assert_eq!((first.width_px, first.height_px), (3840, 2160));
        // Primary Display Only pins the stream to the same pixels and puts
        // the window fullscreen on that display.
        let mut options = options();
        let plan = apply_display_plan_with(
            &mut options,
            &settings,
            DisplaysMode::SinglePrimary,
            None,
            &displays,
        );
        assert_eq!(plan.policy.primary_size(), Some([3840, 2160]));
        assert_eq!(plan.pinned_display, Some(1));
        assert_eq!(options.monitors.len(), 1);
        assert_eq!(
            (options.monitors[0].width_px, options.monitors[0].height_px),
            (3840, 2160)
        );
    }

    #[test]
    fn a_fast_panel_states_its_own_refresh_beside_the_pinned_mode() {
        let mut fast = monitor(1, 0, 1920, 1080, 96, true);
        fast.refresh_hz = Some(143.97);
        let displays = roster_with_metrics(&[fast, monitor(2, 1920, 1920, 1080, 96, false)]);
        let mut options = options();
        let plan = apply_display_plan_with(
            &mut options,
            &ClientSettings::default(),
            DisplaysMode::SinglePrimary,
            None,
            &displays,
        );
        // The virtual display mode stays pinned; the panel's 144 Hz is
        // stated for the host to follow, on the display that shows it.
        assert_eq!(options.monitors[0].refresh_hz, 60);
        let stated = options.display_refresh.clone().expect("stated");
        assert_eq!(stated.stream_display_id, plan.pinned_display);
        assert_eq!(stated.refresh_of(1), Some(144));
        assert_eq!(stated.refresh_of(2), Some(60));
    }

    #[test]
    fn windowed_follows_the_window_and_one_display_needs_no_layout() {
        let settings = ClientSettings::default();
        let displays = two_monitors();
        assert_eq!(
            approve_match_layout_with(DisplaysMode::Windowed, &settings, &displays),
            Ok(None)
        );
        let mut options = options();
        let plan = apply_display_plan_with(
            &mut options,
            &settings,
            DisplaysMode::Windowed,
            None,
            &displays,
        );
        assert!(plan.policy.follows_window());
        assert_eq!(plan.pinned_display, None);
        let one = &displays[..1];
        assert_eq!(
            approve_match_layout_with(DisplaysMode::MatchLayout, &settings, one),
            Ok(None)
        );
    }
}
