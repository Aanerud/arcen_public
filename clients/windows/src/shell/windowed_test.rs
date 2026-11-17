// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Developer-only simulated multi-monitor layout (`dev-tools`), the macOS
//! Deck's `windowed_monitor_test` on Windows.
//!
//! `ARCEN_DECK_WINDOWED_MONITORS` (`2`-`4`, `field`, or a layout list; see
//! `arcen_deck_core::multi_window::simulated_layout`) asks a real Pier for a
//! synthetic layout. Each negotiated monitor is then shown in an ordinary
//! decorated window on this PC's primary display, arranged as the layout is.
//! The parsing, topology, tiling and engine observations are shared. This
//! module only reads the environment once and converts what Windows says of
//! a window. Release packages never contain it
//! (`packaging/windows-deck/build.ps1` refuses the marker below).

use std::sync::OnceLock;

use arcen_deck_core::multi_window::runtime::{ActiveDisplayInfo, ViewportBindObservation};
use arcen_deck_core::multi_window::simulated_layout::{
    self as shared, DisplayBoundsPts, SimulatedLayout, SimulatedLayoutEnv, TilePlacement,
    TilingInsets,
};
use arcen_deck_core::runtime::multi_monitor::RequestedMultiMonitorSelection;
use arcen_media::SessionMonitorId;
use arcen_protocol::messages::SafeAreaPolicyMsg;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::Window;

use super::display_number;

/// The simulated layout of this process, read once.
pub(super) fn layout() -> Option<&'static SimulatedLayout> {
    static LAYOUT: OnceLock<Option<SimulatedLayout>> = OnceLock::new();
    LAYOUT
        .get_or_init(|| match shared::from_process_env() {
            SimulatedLayoutEnv::Off => None,
            SimulatedLayoutEnv::Active(layout) => {
                tracing::warn!(
                    target: "arcen::display",
                    env = shared::MONITORS_ENV_VAR,
                    monitors = layout.len(),
                    "simulated multi-monitor layout active (developer build): each monitor in its own window on this display",
                );
                Some(layout)
            }
            SimulatedLayoutEnv::Invalid(reason) => {
                tracing::error!(
                    target: "arcen::display",
                    env = shared::MONITORS_ENV_VAR,
                    %reason,
                    "simulated multi-monitor layout ignored",
                );
                None
            }
        })
        .as_ref()
}

/// The selection a simulated layout requests: its topology, never this PC's
/// displays.
///
/// # Errors
///
/// The layout breaks a shared topology contract.
pub(super) fn selection(
    layout: &SimulatedLayout,
) -> Result<RequestedMultiMonitorSelection, String> {
    let topology =
        shared::requested_topology(layout).map_err(|error| format!("simulated layout: {error}"))?;
    Ok(RequestedMultiMonitorSelection {
        topology,
        safe_area_policy: SafeAreaPolicyMsg::StandardFullscreen,
        full_color_display_ids: Vec::new(),
    })
}

/// The display that carries the windows: the primary one.
fn backing(event_loop: &ActiveEventLoop) -> Option<(MonitorHandle, DisplayBoundsPts)> {
    let monitor = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next())?;
    let display_id = display_number(&monitor)?;
    let position = monitor.position();
    let size = monitor.size();
    // Physical pixels throughout: winit places and sizes windows in them.
    #[allow(clippy::cast_precision_loss, reason = "desktop coordinates")]
    let bounds = DisplayBoundsPts {
        display_id,
        origin_x: position.x as f32,
        origin_y: position.y as f32,
        width: size.width as f32,
        height: size.height as f32,
    };
    Some((monitor, bounds))
}

/// Where `monitor`'s window goes.
fn tile(
    layout: &SimulatedLayout,
    bounds: DisplayBoundsPts,
    monitor: SessionMonitorId,
) -> Option<TilePlacement> {
    shared::tile_placements(layout, bounds, TilingInsets::default())
        .map_err(|reason| {
            tracing::error!(target: "arcen::display", %reason, "simulated layout cannot be tiled");
        })
        .ok()?
        .into_iter()
        .find(|tile| tile.session_monitor_id == monitor)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "tile coordinates are within the display"
)]
fn physical(tile: TilePlacement) -> (PhysicalPosition<i32>, PhysicalSize<u32>) {
    (
        PhysicalPosition::new(tile.outer_x.round() as i32, tile.outer_y.round() as i32),
        PhysicalSize::new(
            tile.inner_width.round() as u32,
            tile.inner_height.round() as u32,
        ),
    )
}

/// A further window's place: decorated, not fullscreen, at its tile on the
/// backing display. `None` when there is no display to tile on.
pub(super) fn window_attributes(
    event_loop: &ActiveEventLoop,
    layout: &SimulatedLayout,
    monitor: SessionMonitorId,
    title: String,
) -> Option<winit::window::WindowAttributes> {
    let (_, bounds) = backing(event_loop)?;
    let (position, size) = physical(tile(layout, bounds, monitor)?);
    Some(
        Window::default_attributes()
            .with_title(title)
            .with_decorations(true)
            .with_resizable(true)
            .with_position(position)
            .with_inner_size(size),
    )
}

/// Moves the root window to monitor 1's tile once per session.
pub(super) fn place_root(event_loop: &ActiveEventLoop, layout: &SimulatedLayout, root: &Window) {
    let Some((_, bounds)) = backing(event_loop) else {
        return;
    };
    let Ok(first) = SessionMonitorId::new(1) else {
        return;
    };
    let Some(tile) = tile(layout, bounds, first) else {
        return;
    };
    let (position, size) = physical(tile);
    root.set_fullscreen(None);
    root.set_outer_position(position);
    let _ = root.request_inner_size(size);
    root.set_title(&shared::window_title_for(first, layout.len()));
}

/// The engine's view of the displays while the layout is simulated.
pub(super) fn live_displays(
    event_loop: &ActiveEventLoop,
    layout: &SimulatedLayout,
) -> Vec<ActiveDisplayInfo> {
    backing(event_loop)
        .map(|(_, bounds)| shared::synthetic_displays(layout, bounds))
        .unwrap_or_default()
}

/// What a window shown on the backing display tells the engine: bound on
/// its synthetic display `expected`.
pub(super) fn observation(
    event_loop: &ActiveEventLoop,
    observed: ViewportBindObservation,
    expected: u32,
) -> ViewportBindObservation {
    match backing(event_loop) {
        Some((_, bounds)) => {
            shared::confirmation_observation(observed, expected, bounds.display_id)
        }
        None => observed,
    }
}
