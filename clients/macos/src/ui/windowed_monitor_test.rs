// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Developer-only windowed multi-monitor live-session test mode.
//!
//! `ARCEN_DECK_WINDOWED_MONITORS` (`2`-`4`, `field`, or a layout list; see
//! `arcen_deck_core::multi_window::simulated_layout`, shared with the Windows
//! Deck) asks a real Pier for a synthetic layout while presenting each
//! negotiated monitor in an ordinary decorated, resizable, non-fullscreen
//! window on one local display, arranged as the layout is. This is
//! compiled only with `dev-tools`: ordinary release/package builds contain no
//! parser, environment variable, synthetic topology, or alternate window
//! placement.

use arcen_deck_core::multi_window::simulated_layout::{self as shared, SimulatedLayout};
use arcen_media::{RequestedMonitorTopology, SessionMonitorId};

use crate::protocol::messages::ClientMonitor;

pub use shared::{DisplayBoundsPts, TilePlacement, TilingInsets, MONITORS_ENV_VAR};

/// The active simulated layout (`Copy`, as the shell's window state is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowedMonitorTestMode {
    pub monitor_count: usize,
    /// The first monitor's size, for logs.
    pub monitor_size_px: [u32; 2],
    pub layout: SimulatedLayout,
}

impl WindowedMonitorTestMode {
    /// `count` monitors of 1920x1080 (the `2`-`4` form).
    #[must_use]
    pub fn default_size(count: usize) -> Self {
        match parse_env(Some(&count.to_string()), None) {
            WindowedMonitorTestEnv::Active(mode) => mode,
            _ => panic!("{count} is not a simulated monitor count"),
        }
    }

    fn from_layout(layout: SimulatedLayout) -> Self {
        let first = layout.monitors()[0];
        Self {
            monitor_count: layout.len(),
            monitor_size_px: [first.width, first.height],
            layout,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowedMonitorTestEnv {
    Off,
    Active(WindowedMonitorTestMode),
    Invalid(String),
}

#[must_use]
pub fn parse_env(monitors: Option<&str>, size: Option<&str>) -> WindowedMonitorTestEnv {
    match shared::parse_env(monitors, size) {
        shared::SimulatedLayoutEnv::Off => WindowedMonitorTestEnv::Off,
        shared::SimulatedLayoutEnv::Active(layout) => {
            WindowedMonitorTestEnv::Active(WindowedMonitorTestMode::from_layout(layout))
        }
        shared::SimulatedLayoutEnv::Invalid(reason) => WindowedMonitorTestEnv::Invalid(reason),
    }
}

#[must_use]
pub fn mode_from_process_env() -> WindowedMonitorTestEnv {
    parse_env(
        std::env::var(shared::MONITORS_ENV_VAR).ok().as_deref(),
        std::env::var(shared::MONITOR_SIZE_ENV_VAR).ok().as_deref(),
    )
}

#[must_use]
pub const fn synthetic_display_id(index: usize) -> u32 {
    shared::synthetic_display_id(index)
}

#[must_use]
pub fn root_window_title(mode: WindowedMonitorTestMode) -> String {
    window_title_for(
        SessionMonitorId::new(1).expect("monitor 1"),
        mode.monitor_count,
    )
}

#[must_use]
pub fn window_title_for(session_monitor_id: SessionMonitorId, total: usize) -> String {
    shared::window_title_for(session_monitor_id, total)
}

pub fn synthetic_requested_topology(
    mode: WindowedMonitorTestMode,
) -> Result<RequestedMonitorTopology, arcen_media::MediaContractError> {
    shared::requested_topology(&mode.layout)
}

#[must_use]
pub fn client_monitors_from_synthetic_topology(
    topology: &RequestedMonitorTopology,
) -> Vec<ClientMonitor> {
    arcen_deck_core::connect_plan::client_monitors_from_topology(topology)
}

pub fn tile_placements(
    mode: WindowedMonitorTestMode,
    display: DisplayBoundsPts,
    insets: TilingInsets,
) -> Result<Vec<TilePlacement>, String> {
    shared::tile_placements(&mode.layout, display, insets)
}

pub fn placement_for_monitor(
    mode: WindowedMonitorTestMode,
    display: DisplayBoundsPts,
    monitor_id: SessionMonitorId,
) -> Result<TilePlacement, String> {
    tile_placements(mode, display, TilingInsets::default())?
        .into_iter()
        .find(|tile| tile.session_monitor_id == monitor_id)
        .ok_or_else(|| format!("no window for monitor {}", monitor_id.get()))
}

#[must_use]
pub fn confirmation_observation(
    observation: crate::ui::multi_window_runtime::ViewportBindObservation,
    expected_virtual_display_id: u32,
    backing_display_id: u32,
) -> crate::ui::multi_window_runtime::ViewportBindObservation {
    shared::confirmation_observation(observation, expected_virtual_display_id, backing_display_id)
}

#[cfg(target_os = "macos")]
pub fn primary_display_bounds() -> Option<DisplayBoundsPts> {
    let info = crate::ui::multi_window_runtime::live_active_displays()
        .into_iter()
        .next()?;
    let bounds = core_graphics::display::CGDisplay::new(info.display_id).bounds();
    Some(DisplayBoundsPts {
        display_id: info.display_id,
        origin_x: bounds.origin.x as f32,
        origin_y: bounds.origin.y as f32,
        width: bounds.size.width as f32,
        height: bounds.size.height as f32,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn primary_display_bounds() -> Option<DisplayBoundsPts> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::multi_window_runtime::build_plan;

    fn display() -> DisplayBoundsPts {
        DisplayBoundsPts {
            display_id: 42,
            origin_x: 10.0,
            origin_y: 20.0,
            width: 1_800.0,
            height: 1_200.0,
        }
    }

    fn active(env: WindowedMonitorTestEnv) -> WindowedMonitorTestMode {
        match env {
            WindowedMonitorTestEnv::Active(mode) => mode,
            other => panic!("not active: {other:?}"),
        }
    }

    #[test]
    fn counts_and_the_field_layout_parse_through_the_shared_layout() {
        assert_eq!(parse_env(None, None), WindowedMonitorTestEnv::Off);
        let two = active(parse_env(Some("2"), None));
        assert_eq!(
            (two.monitor_count, two.monitor_size_px),
            (2, [1_920, 1_080])
        );
        let field = active(parse_env(Some("field"), None));
        assert_eq!(field.monitor_size_px, [2_560, 1_600]);
        assert!(matches!(
            parse_env(Some("1"), None),
            WindowedMonitorTestEnv::Invalid(_)
        ));
    }

    #[test]
    fn synthetic_layout_is_primary_first_with_unique_virtual_ids() {
        let mode = active(parse_env(Some("3"), Some("1280x720")));
        let topology = synthetic_requested_topology(mode).expect("synthetic topology");
        assert_eq!(topology.monitors().len(), 3);
        assert!(topology.monitors()[0].monitor().primary);
        let monitors = client_monitors_from_synthetic_topology(&topology);
        assert_eq!(monitors[1].id, synthetic_display_id(1));
    }

    #[test]
    fn tiles_follow_the_layout() {
        let field = active(parse_env(Some("field"), None));
        let tiles = tile_placements(field, display(), TilingInsets::default()).expect("tiles");
        assert!(tiles[1].outer_x < tiles[0].outer_x && tiles[1].outer_y > tiles[0].outer_y);
        let placement = placement_for_monitor(field, display(), SessionMonitorId::new(2).unwrap())
            .expect("tile");
        assert_eq!(placement.index, 1);
    }

    #[test]
    fn production_plan_still_rejects_same_display_windows() {
        let ids = [
            SessionMonitorId::new(1).unwrap(),
            SessionMonitorId::new(2).unwrap(),
        ];
        assert!(
            build_plan(&ids, &[42, 42]).is_err(),
            "dev-tools windowed mode must not weaken production duplicate-display invariants"
        );
    }
}
